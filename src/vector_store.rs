use eyre::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VectorEntry {
    pub id: String,
    pub file_hash: String,
    pub embedding: Vec<f32>,
    pub content_preview: String,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct VectorStore {
    pub entries: Vec<VectorEntry>,
    #[serde(skip)]
    pub path: PathBuf,
}

impl VectorStore {
    pub async fn load(path: PathBuf) -> Result<Self> {
        if path.exists() {
            let content = fs::read_to_string(&path).await?;
            let mut store: VectorStore = serde_json::from_str(&content)?;
            store.path = path;
            Ok(store)
        } else {
            Ok(VectorStore {
                entries: Vec::new(),
                path,
            })
        }
    }

    pub async fn save(&self) -> Result<()> {
        let content = serde_json::to_string_pretty(&self)?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&self.path, content).await?;
        Ok(())
    }

    pub async fn add(&mut self, entry: VectorEntry) -> Result<()> {
        self.entries.push(entry);
        self.save().await?;
        Ok(())
    }

    pub fn search(&self, query_embedding: &[f32], limit: usize) -> Vec<(&VectorEntry, f32)> {
        let mut scored_entries: Vec<(&VectorEntry, f32)> = self
            .entries
            .iter()
            .map(|entry| {
                let score = cosine_similarity(&entry.embedding, query_embedding);
                (entry, score)
            })
            .collect();

        scored_entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored_entries.truncate(limit);
        scored_entries
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot_product: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let magnitude_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let magnitude_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if magnitude_a == 0.0 || magnitude_b == 0.0 {
        0.0
    } else {
        dot_product / (magnitude_a * magnitude_b)
    }
}
