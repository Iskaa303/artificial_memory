use log::{debug, trace, warn};
use std::collections::BTreeSet;
use std::path::PathBuf;
use tokio::fs as tfs;

#[derive(Default, Debug)]
pub struct FileStorage {
    paths: BTreeSet<PathBuf>,
}

impl FileStorage {
    pub fn new() -> Self {
        trace!("Initializing new FileStorage");
        Self::default()
    }

    pub async fn add(&mut self, path: impl Into<PathBuf>) -> &mut Self {
        self.add_recursive(path.into()).await;
        self
    }

    async fn add_recursive(&mut self, path: PathBuf) {
        if !path.exists() {
            warn!("Path does not exist: {}", path.display());
            return;
        }

        if tfs::metadata(&path)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            if let Ok(mut entries) = tfs::read_dir(&path).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    Box::pin(self.add_recursive(entry.path())).await;
                }
            }
        } else {
            let target_path = match tfs::canonicalize(&path).await {
                Ok(canonical) => canonical,
                Err(_) => path,
            };
            debug!("Adding file: {}", target_path.display());
            self.paths.insert(target_path);
        }
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn paths(&self) -> &BTreeSet<PathBuf> {
        &self.paths
    }
}
