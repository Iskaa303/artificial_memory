use crate::vector_store::{VectorEntry, VectorStore};
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use eyre::{Result, eyre};
use hf_hub::{Repo, RepoType, api::tokio::Api};
use sha2::{Digest, Sha256};
use std::path::Path;
use tokenizers::Tokenizer;
use tokio::fs;

pub struct Digester {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl Digester {
    pub async fn new() -> Result<Self> {
        let device = Device::Cpu;
        let api = Api::new()?;
        let repo = api.repo(Repo::new(
            "sentence-transformers/all-MiniLM-L6-v2".to_string(),
            RepoType::Model,
        ));

        let config_filename = repo.get("config.json").await?;
        let tokenizer_filename = repo.get("tokenizer.json").await?;
        let weights_filename = repo.get("model.safetensors").await?;

        let config: Config = serde_json::from_str(&fs::read_to_string(config_filename).await?)?;
        let tokenizer = Tokenizer::from_file(tokenizer_filename).map_err(|e| eyre!(e))?;

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(
                &[weights_filename],
                candle_core::DType::F32,
                &device,
            )?
        };
        let model = BertModel::load(vb, &config)?;

        Ok(Self {
            model,
            tokenizer,
            device,
        })
    }

    pub async fn digest_file(
        &self,
        file_path: &Path,
        vector_store: &mut VectorStore,
    ) -> Result<()> {
        let content = fs::read_to_string(file_path).await?;
        let embedding = self.generate_embedding(&content)?;

        let file_name = file_path
            .file_name()
            .ok_or_else(|| eyre!("Invalid file path"))?
            .to_string_lossy()
            .to_string();

        let mut hasher = Sha256::new();
        hasher.update(&content);
        let id = format!("{:x}", hasher.finalize());

        let entry = VectorEntry {
            id,
            file_hash: file_name,
            embedding,
            content_preview: content.chars().take(100).collect(),
        };

        vector_store.add(entry).await?;
        Ok(())
    }

    fn generate_embedding(&self, text: &str) -> Result<Vec<f32>> {
        let tokens = self.tokenizer.encode(text, true).map_err(|e| eyre!(e))?;

        let token_ids = Tensor::new(tokens.get_ids(), &self.device)?.unsqueeze(0)?;
        let token_type_ids = Tensor::new(tokens.get_type_ids(), &self.device)?.unsqueeze(0)?;

        let embeddings = self.model.forward(&token_ids, &token_type_ids, None)?;

        let (_n_sentence, n_tokens, _hidden_size) = embeddings.dims3()?;
        let embeddings = (embeddings.sum(1)? / (n_tokens as f64))?;
        let embeddings = embeddings.get(0)?;

        let vec: Vec<f32> = embeddings.to_vec1()?;
        Ok(vec)
    }
}
