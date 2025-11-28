use chrono::{DateTime, Local};
use eyre::{Result, eyre};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::SystemTime;
use tokio::fs;
use tokio::io::AsyncReadExt;

#[derive(Serialize, Deserialize, Debug)]
pub struct FileMetadata {
    pub name: String,
    pub size: u64,
    pub hash: String,
    pub created: Option<u64>,
    pub created_readable: Option<String>,
    pub modified: Option<u64>,
    pub modified_readable: Option<String>,
}

pub async fn store_file(source: &Path) -> Result<()> {
    let file_name = source
        .file_name()
        .ok_or_else(|| eyre!("Invalid source path"))?
        .to_string_lossy()
        .to_string();

    let mut file = fs::File::open(source).await?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).await?;
    let mut hasher = Sha256::new();
    hasher.update(&buffer);
    let hash = format!("{:x}", hasher.finalize());

    let metadata = fs::metadata(source).await?;

    let created_time = metadata.created().ok();
    let created = created_time
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let created_readable = created_time.map(|t| {
        DateTime::<Local>::from(t)
            .format("%Y-%m-%d %H:%M:%S %A %z")
            .to_string()
    });

    let modified_time = metadata.modified().ok();
    let modified = modified_time
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let modified_readable = modified_time.map(|t| {
        DateTime::<Local>::from(t)
            .format("%Y-%m-%d %H:%M:%S %A %z")
            .to_string()
    });

    let file_meta = FileMetadata {
        name: file_name.clone(),
        size: metadata.len(),
        hash: hash.clone(),
        created,
        created_readable,
        modified,
        modified_readable,
    };

    let dest_dir = Path::new("memory").join(format!("{file_name}-{hash}"));
    if !dest_dir.exists() {
        fs::create_dir_all(&dest_dir).await?;
    }

    let dest_file_path = dest_dir.join(&file_name);
    fs::copy(source, &dest_file_path).await?;

    let metadata_path = dest_dir.join("metadata.json");
    let metadata_json = serde_json::to_string_pretty(&file_meta)?;
    fs::write(metadata_path, metadata_json).await?;

    Ok(())
}
