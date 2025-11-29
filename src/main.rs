use eyre::Result;
use ouroboros::{digest, ingest, vector_store::VectorStore};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    let test_path = PathBuf::from("test.txt");

    println!("Ingesting file to Intermediate Memory...");
    ingest::process(test_path.clone()).await?;
    println!("Processed file: {:?}", test_path);

    println!("Digesting file to Final Memory...");
    let mut vector_store = VectorStore::load(PathBuf::from("memory/vectors.json")).await?;
    let digester = digest::Digester::new().await?;

    digester.digest_file(&test_path, &mut vector_store).await?;
    println!("Digested file to Final Memory.");

    println!("Searching for 'test'...");
    println!(
        "Vector store contains {} entries.",
        vector_store.entries.len()
    );
    if let Some(entry) = vector_store.entries.first() {
        println!("First entry ID: {}", entry.id);
        println!("First entry Preview: {}", entry.content_preview);
    }

    Ok(())
}
