use eyre::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::{debug, error, info, trace, warn};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::{TextDiff, udiff::UnifiedDiff};
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use thiserror::Error;

const CHUNK_SIZE: usize = 8 * 1024 * 1024; // 8MB

#[derive(Error, Debug)]
pub enum ProcessError {
    #[error("failed to create directory: {0}")]
    CreateDir(PathBuf),
    #[error("failed to handle file: {0}")]
    File(PathBuf),
    #[error("failed to write metadata: {0}")]
    Metadata(PathBuf),
}

#[derive(Serialize, Deserialize, Default)]
struct FileHistory {
    versions: Vec<FileVersion>,
    alias: String,
    original_path: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct FileVersion {
    version: u32,
    hash: String,
    size: u64,
    mtime_ns: u128,
    processed_at: String,
    diff_file: Option<String>,
}

pub struct Processor;

impl Processor {
    pub async fn process_all(paths: &BTreeSet<PathBuf>) -> Result<()> {
        let memory_dir = PathBuf::from("memory");
        if !memory_dir.exists() {
            fs::create_dir_all(&memory_dir).context("Failed to create memory directory")?;
        }

        let paths_vec: Vec<_> = paths.iter().cloned().collect();
        info!(
            "Starting parallel async processing of {} files",
            paths_vec.len()
        );

        let mut handles = Vec::new();
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(16)); // Limit concurrent I/O to 16
        let multi = std::sync::Arc::new(MultiProgress::new());

        for path in paths_vec {
            let memory_dir = memory_dir.clone();
            let semaphore = semaphore.clone();
            let multi = multi.clone();
            handles.push(tokio::spawn(async move {
                Self::pipeline_file(path, memory_dir, semaphore, multi).await
            }));
        }

        for handle in handles {
            if let Err(e) = handle.await.wrap_err("Task panicked")? {
                error!("A processing task failed: {:?}", e);
                return Err(e);
            }
        }

        info!("Finished all processing tasks.");
        Ok(())
    }

    async fn pipeline_file(
        path: PathBuf,
        memory_dir: PathBuf,
        semaphore: std::sync::Arc<tokio::sync::Semaphore>,
        multi: std::sync::Arc<MultiProgress>,
    ) -> Result<()> {
        let path_ref = &path;
        let metadata = tokio::fs::metadata(path_ref).await.map_err(|e| {
            error!("Failed to get metadata for {}: {}", path_ref.display(), e);
            ProcessError::File(path_ref.to_path_buf())
        })?;
        let current_size = metadata.len();
        let current_mtime = metadata
            .modified()
            .map_err(|_| ProcessError::File(path.to_path_buf()))?
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();

        let path_alias = Self::calculate_path_alias(&path);
        let target_dir = memory_dir.join(&path_alias);
        let file_basename = path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();

        if !target_dir.exists() {
            tokio::fs::create_dir_all(&target_dir).await.map_err(|e| {
                error!(
                    "Failed to create target dir {}: {}",
                    target_dir.display(),
                    e
                );
                ProcessError::CreateDir(target_dir.clone())
            })?;
        }

        let history_path = target_dir.join("history.json");
        let mut history = if history_path.exists() {
            let data = tokio::fs::read_to_string(&history_path)
                .await
                .map_err(|e| {
                    error!(
                        "Failed to read history file {}: {}",
                        history_path.display(),
                        e
                    );
                    ProcessError::Metadata(history_path.clone())
                })?;
            serde_json::from_str(&data).unwrap_or_else(|_| {
                warn!(
                    "Failed to parse history.json for {}. Recreating.",
                    path.display()
                );
                FileHistory {
                    alias: path_alias.clone(),
                    original_path: path.to_string_lossy().to_string(),
                    ..Default::default()
                }
            })
        } else {
            FileHistory {
                alias: path_alias.clone(),
                original_path: path.to_string_lossy().to_string(),
                ..Default::default()
            }
        };

        if history
            .versions
            .last()
            .is_some_and(|l| l.size == current_size && l.mtime_ns == current_mtime)
        {
            trace!("[{}] Skipping unchanged file (metadata).", file_basename);
            return Ok(());
        }

        let _permit = semaphore
            .acquire()
            .await
            .wrap_err("Failed to acquire semaphore")?;
        let current_hash = Self::process_file_stream(&path, multi).await?;

        // Deep change detection
        if history
            .versions
            .last()
            .is_some_and(|l| l.hash == current_hash)
        {
            trace!("[{}] Skipping unchanged file (content).", file_basename);
            return Ok(());
        }

        // Diff and Storage Stage
        let latest_file_path = target_dir.join("latest");
        let mut diff_filename = None;

        if latest_file_path.exists() && current_size < CHUNK_SIZE as u64 {
            if let Ok(source_content) = tokio::fs::read_to_string(&path).await {
                if let Ok(old_content) = tokio::fs::read_to_string(&latest_file_path).await {
                    let next_v = history.versions.len() + 1;
                    let diff_name = format!("v{}.diff", next_v);
                    let diff_path = target_dir.join(&diff_name);

                    let text_diff = TextDiff::from_lines(&old_content, &source_content);
                    let diff_text = UnifiedDiff::from_text_diff(&text_diff)
                        .header(file_basename.as_ref(), file_basename.as_ref())
                        .to_string();

                    if !diff_text.is_empty() {
                        tokio::fs::write(&diff_path, diff_text).await.map_err(|e| {
                            error!("Failed to write diff file {}: {}", diff_path.display(), e);
                            ProcessError::File(diff_path)
                        })?;
                        diff_filename = Some(diff_name);
                    }
                } else {
                    debug!(
                        "Could not read old content from {} for diffing.",
                        latest_file_path.display()
                    );
                }
            } else {
                debug!(
                    "Could not read source content from {} for diffing.",
                    path.display()
                );
            }
        }

        // Finalize: Update latest and record version
        let temp_latest = target_dir.join("latest.tmp");
        tokio::fs::copy(&path, &temp_latest).await.map_err(|e| {
            error!(
                "Failed to copy {} to {}: {}",
                path.display(),
                temp_latest.display(),
                e
            );
            ProcessError::File(temp_latest.clone())
        })?;
        tokio::fs::rename(&temp_latest, &latest_file_path)
            .await
            .map_err(|e| {
                error!(
                    "Failed to rename {} to {}: {}",
                    temp_latest.display(),
                    latest_file_path.display(),
                    e
                );
                ProcessError::File(latest_file_path)
            })?;

        let next_version = history.versions.len() as u32 + 1;
        history.versions.push(FileVersion {
            version: next_version,
            hash: current_hash,
            size: current_size,
            mtime_ns: current_mtime,
            processed_at: chrono::Local::now().to_rfc3339(),
            diff_file: diff_filename,
        });

        let history_json =
            serde_json::to_string_pretty(&history).wrap_err("Failed to serialize history")?;
        tokio::fs::write(&history_path, history_json)
            .await
            .map_err(|e| {
                error!(
                    "Failed to write history file {}: {}",
                    history_path.display(),
                    e
                );
                ProcessError::Metadata(history_path)
            })?;

        info!("[{}] Version v{} stored.", file_basename, next_version);
        Ok(())
    }

    async fn process_file_stream(
        path: &Path,
        multi: std::sync::Arc<MultiProgress>,
    ) -> Result<String> {
        let metadata = fs::metadata(path).map_err(|_| ProcessError::File(path.to_path_buf()))?;
        let file_size = metadata.len();

        let file_basename = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let pb = multi.add(ProgressBar::new(file_size));
        pb.set_style(ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({percent}%) {msg}")?
            .progress_chars("#>-"));
        pb.set_message(file_basename.clone());

        let path_buf = path.to_path_buf();
        let pb_inner = pb.clone();
        let hash = tokio::task::spawn_blocking(move || -> Result<String> {
            let mut f =
                fs::File::open(&path_buf).map_err(|_| ProcessError::File(path_buf.clone()))?;

            let mut hasher = Sha256::new();
            let mut buffer = vec![0u8; CHUNK_SIZE];

            loop {
                let n = f
                    .read(&mut buffer)
                    .map_err(|_| ProcessError::File(path_buf.clone()))?;
                if n == 0 {
                    break;
                }

                let chunk = &buffer[..n];
                // Efficient \r filtering: find segments between \r and update hasher with slices
                let mut start = 0;
                while let Some(pos) = chunk[start..].iter().position(|&b| b == b'\r') {
                    let actual_pos = start + pos;
                    hasher.update(&chunk[start..actual_pos]);
                    start = actual_pos + 1;
                }
                hasher.update(&chunk[start..]);

                pb_inner.inc(n as u64);
            }

            Ok(format!("{:x}", hasher.finalize()))
        })
        .await
        .wrap_err("Hashing task panicked")??;

        pb.finish_with_message(format!("{} [DONE]", file_basename));
        Ok(hash)
    }

    fn calculate_path_alias(path: &Path) -> String {
        let path_str = path.to_string_lossy().replace("\\", "/");
        let path_clean = path_str.trim_start_matches("//?/");

        let normalized = if let Ok(cwd) = std::env::current_dir() {
            let cwd_str = cwd.to_string_lossy().replace("\\", "/");
            let cwd_clean = cwd_str.trim_start_matches("//?/");

            if let Some(rel) = path_clean.strip_prefix(cwd_clean) {
                rel.trim_start_matches('/')
            } else {
                path_clean
            }
        } else {
            path_clean
        };

        normalized
            .replace(":", "_")
            .replace("/", "_")
            .replace(" ", "_")
            .to_lowercase()
    }
}
