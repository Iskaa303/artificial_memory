mod process;
mod logger;
mod storage;
mod environment;
mod agent;

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

    let workspace_root = std::env::current_dir()?;
    let env_manager = environment::WasmEnvironmentManager::new("memory", &workspace_root)?;

    if let Err(e) = Processor::process_all(storage.paths()).await {
        error!("Fatal error during processing: {:?}", e);
        std::process::exit(1);
    }

    let envs_prepared = match env_manager.create_environments_for_memory_entries().await {
        Ok(_) => true,
        Err(e) => {
            warn!(
                "Failed to prepare per-memory-entry isolated environments after processing: {:?}",
                e
            );
            false
        }
    };

    info!("Starting agent swarm over memory entries");
    if let Err(e) = agent::generate_simple_memory_reports(
        &workspace_root,
        &env_manager,
        agent::MemoryReportTask {
            task_description: "\
                Use REPL on demand. If you need file contents, output exactly one line:\n\
                REPL_READ:<path>\n\
                Then wait for tool results and continue.\n\
                Final output must be a compact report for this entry including:\n\
                1) What the file is.\n\
                2) What its contents are about.\n\
                3) A metadata section using the REPL metadata provided above.\n\
                4) A content explanation that is exactly 100 words."
                .to_string(),
        },
    )
    .await
    {
        warn!("LLM reporting failed: {:?}", e);
    } else {
        info!("Agent swarm completed");
    }

    if !envs_prepared {
        if let Err(e) = env_manager.create_environments_for_memory_entries().await {
            warn!("Late retry to prepare environments also failed: {:?}", e);
        }
    }

    Ok(())
}
