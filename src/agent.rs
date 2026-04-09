use chrono::Utc;
use eyre::{Context, Result, bail, eyre};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::time::{Duration, timeout};

use crate::environment::{EnvironmentCommand, WasmEnvironmentManager};

const CONFIG_PATH: &str = "config/agent.toml";
const MAX_AGENT_STEPS: usize = 4;
const MAX_REPL_READ_CHARS: usize = 12_000;
const LLM_STEP_TIMEOUT_SECS: u64 = 120;

pub async fn generate_simple_memory_reports(
    workspace_root: &Path,
    env_manager: &WasmEnvironmentManager,
    task: MemoryReportTask,
) -> Result<()> {
    let orchestrator = LlamaCppAgentOrchestrator::from_workspace(workspace_root).await?;
    orchestrator.run_if_enabled(env_manager, &task).await
}

/// Task description passed from the call-site (e.g. `main.rs`) to the agent swarm.
/// Agents use this to know what to produce for each memory entry.
pub struct MemoryReportTask {
    pub task_description: String,
}

#[derive(Debug, Clone)]
struct EntryMetadata {
    history_versions: usize,
    latest_bytes: usize,
    latest_chars: usize,
    latest_lines: usize,
    latest_modified_at: String,
    repl_metadata: String,
}

pub struct LlamaCppAgentOrchestrator {
    workspace_root: PathBuf,
    config: AgentConfig,
}

impl LlamaCppAgentOrchestrator {
    pub async fn from_workspace(workspace_root: &Path) -> Result<Self> {
        let config_path = workspace_root.join(CONFIG_PATH);
        ensure_default_config_if_missing(&config_path).await?;

        let raw = tokio::fs::read_to_string(&config_path)
            .await
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let config: AgentConfig = toml::from_str(&raw)
            .with_context(|| format!("failed to parse {}", config_path.display()))?;

        Ok(Self {
            workspace_root: workspace_root.to_path_buf(),
            config,
        })
    }

    pub async fn run_if_enabled(&self, env_manager: &WasmEnvironmentManager, task: &MemoryReportTask) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }

        let llm_url = self
            .config
            .runtime
            .llm_url
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string);
        let api_type_raw = self
            .config
            .runtime
            .api_type
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string);
        let model_name = self
            .config
            .model
            .name
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string);

        let mut missing = Vec::new();
        if llm_url.is_none() {
            missing.push("runtime.llm_url");
        }
        if api_type_raw.is_none() {
            missing.push("runtime.api_type");
        }
        if model_name.is_none() {
            missing.push("model.name");
        }
        if !missing.is_empty() {
            warn!(
                "AI support is disabled because config is missing: {}",
                missing.join(", ")
            );
            warn!("Set these in config/agent.toml and rerun:");
            warn!("  [runtime] llm_url = \"http://127.0.0.1:8080\"");
            warn!("  [runtime] api_type = \"llama\"  # or \"openai\"");
            warn!("  [model]   name = \"your-model-name\"");
            warn!("Then rerun: cargo run --release");
            return Ok(());
        }

        let api_type = match LlmApiType::parse(api_type_raw.as_deref().unwrap_or_default()) {
            Some(value) => value,
            None => {
                warn!(
                    "Unsupported runtime.api_type '{}'. Supported values: 'llama', 'openai'",
                    api_type_raw.unwrap_or_default()
                );
                warn!("Fix config/agent.toml and rerun: cargo run --release");
                return Ok(());
            }
        };

        let llm_url = llm_url.unwrap_or_default();
        let model_name = model_name.unwrap_or_default();
        let client = reqwest::Client::builder()
            .build()
            .context("failed to create HTTP client for LLM URL")?;
        info!(
            "Using remote LLM endpoint: api_type={}, url={}, model={}",
            api_type.as_str(),
            llm_url,
            model_name
        );

        let folders = self.list_memory_entry_folders().await?;
        info!("Agent swarm started for {} memory entries", folders.len());

        for (idx, folder) in folders.iter().enumerate() {
            info!("Agent [{}/{}] preparing entry '{}'", idx + 1, folders.len(), folder);
            let manifest = env_manager
                .create_or_get_environment_for_memory_folder(folder, &[])
                .await?;

            let metadata = fetch_entry_metadata_via_repl(env_manager, &manifest.name)
                .await
                .unwrap_or_else(|e| EntryMetadata {
                    history_versions: 0,
                    latest_bytes: 0,
                    latest_chars: 0,
                    latest_lines: 0,
                    latest_modified_at: "unavailable".to_string(),
                    repl_metadata: format!("metadata_fetch_error={e}"),
                });

            let mut tool_context = String::new();
            let mut final_answer = String::new();
            let mut last_output = String::new();

            for step in 1..=MAX_AGENT_STEPS {
                info!("Agent [{}] step {}/{} running inference", folder, step, MAX_AGENT_STEPS);
                let prompt = self.build_prompt(
                    folder,
                    &model_name,
                    api_type,
                    &metadata,
                    &task.task_description,
                    &tool_context,
                    step,
                );
                let answer = run_llm_via_url(
                    &client,
                    &llm_url,
                    api_type,
                    &model_name,
                    self.config.inference.n_predict,
                    self.config.inference.temperature,
                    &prompt,
                )
                .await?;
                last_output = answer.clone();

                if let Some(path) = parse_repl_read_request(&answer) {
                    info!("Agent [{}] requested REPL_READ:{}", folder, path);
                    let content = read_path_via_repl(env_manager, &manifest.name, &path)
                        .await
                        .unwrap_or_else(|e| format!("REPL_READ error: {}", e));
                    tool_context.push_str(&format!(
                        "\nTOOL_RESULT path={} chars={}\n{}\n",
                        path,
                        content.chars().count(),
                        content
                    ));
                    continue;
                }

                final_answer = answer;
                break;
            }

            let answer = if final_answer.trim().is_empty() {
                last_output
            } else {
                final_answer
            };

            let report_content = render_report(folder, &metadata, &answer);

            env_manager
                .send_command(
                    &manifest.name,
                    EnvironmentCommand::WriteFile {
                        path: self.config.report_file.clone(),
                        content: report_content,
                    },
                )
                .await?;
            info!("Agent [{}] wrote report to {}", folder, self.config.report_file);

            if self.config.save_transcript {
                let transcript_name = format!(
                    "agent/dialog-{}.txt",
                    Utc::now().format("%Y%m%d-%H%M%S")
                );
                env_manager
                    .send_command(
                        &manifest.name,
                        EnvironmentCommand::WriteFile {
                            path: transcript_name,
                            content: answer,
                        },
                    )
                    .await?;
            }
        }

        info!("Agent swarm finished");
        Ok(())
    }

    fn build_prompt(
        &self,
        folder: &str,
        model_name: &str,
        api_type: LlmApiType,
        meta: &EntryMetadata,
        task_description: &str,
        tool_context: &str,
        step: usize,
    ) -> String {
        format!(
            "{system}\n\nModel name: {model_name}\nAPI type: {api_type}\nMemory entry folder: {folder}\n\nRust metadata:\n- versions={versions}\n- latest_bytes={bytes}\n- latest_chars={chars}\n- latest_lines={lines}\n- latest_modified_at={mtime}\n\nREPL metadata:\n{repl}\n\nTool access contract:\n- You can request file content by outputting EXACTLY one line: REPL_READ:<path>\n- Allowed useful paths include: ../fs/latest and ../fs/history.json\n- If you request REPL_READ, output only that one command line and nothing else.\n- When you have enough context, output the final report directly.\n\nCurrent tool results for this entry:\n{tool_context}\n\nCurrent step: {step}\n\nTask:\n{task}",
            system   = self.config.system_prompt,
            model_name = model_name,
            api_type = api_type.as_str(),
            folder   = folder,
            versions = meta.history_versions,
            bytes    = meta.latest_bytes,
            chars    = meta.latest_chars,
            lines    = meta.latest_lines,
            mtime    = meta.latest_modified_at,
            repl     = meta.repl_metadata,
            tool_context = tool_context,
            step = step,
            task     = task_description,
        )
    }

    async fn list_memory_entry_folders(&self) -> Result<Vec<String>> {
        let memory_root = self.workspace_root.join("memory");
        let mut folders = Vec::new();
        let mut dir = tokio::fs::read_dir(&memory_root)
            .await
            .with_context(|| format!("failed to read {}", memory_root.display()))?;

        while let Some(entry) = dir.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name != "environment" {
                    folders.push(name);
                }
            }
        }

        folders.sort();
        Ok(folders)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_system_prompt")]
    pub system_prompt: String,
    #[serde(default = "default_save_transcript")]
    pub save_transcript: bool,
    #[serde(default = "default_report_file")]
    pub report_file: String,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub model: ModelConfig,
    #[serde(default)]
    pub inference: InferenceConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            system_prompt: default_system_prompt(),
            save_transcript: default_save_transcript(),
            report_file: default_report_file(),
            runtime: RuntimeConfig::default(),
            model: ModelConfig::default(),
            inference: InferenceConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub llm_url: Option<String>,
    #[serde(default)]
    pub api_type: Option<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            llm_url: None,
            api_type: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum LlmApiType {
    Llama,
    OpenAi,
}

impl LlmApiType {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "llama" | "llama.cpp" | "llama-cpp" => Some(Self::Llama),
            "openai" | "openai-compatible" => Some(Self::OpenAi),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Llama => "llama",
            Self::OpenAi => "openai",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default)]
    pub name: Option<String>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            name: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceConfig {
    #[serde(default = "default_n_predict")]
    pub n_predict: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            n_predict: default_n_predict(),
            temperature: default_temperature(),
        }
    }
}

fn default_save_transcript() -> bool {
    true
}

fn default_report_file() -> String {
    "agent/entry-report.txt".to_string()
}

fn default_system_prompt() -> String {
    "You are a coding agent connected to a remote LLM endpoint. Use memory history and latest snapshot precisely."
        .to_string()
}

fn default_n_predict() -> u32 {
    384
}

fn default_temperature() -> f32 {
    0.2
}


async fn ensure_default_config_if_missing(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let default_config = AgentConfig::default();
    let raw = toml::to_string_pretty(&default_config).context("failed to render default agent config")?;

    tokio::fs::write(path, raw)
        .await
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(())
}
fn render_report(folder: &str, metadata: &EntryMetadata, answer: &str) -> String {
    format!(
        "Memory entry report\nentry: {}\ncreated_at: {}\n\nRust metadata\n- history_versions: {}\n- latest_bytes: {}\n- latest_chars: {}\n- latest_lines: {}\n- latest_modified_at: {}\n\nREPL metadata\n{}\n\nLLM summary\n{}\n",
        folder,
        Utc::now().to_rfc3339(),
        metadata.history_versions,
        metadata.latest_bytes,
        metadata.latest_chars,
        metadata.latest_lines,
        metadata.latest_modified_at,
        metadata.repl_metadata,
        answer
    )
}

/// Uses the Rust REPL inside the environment sandbox to read lightweight metadata.
async fn fetch_entry_metadata_via_repl(
    env_manager: &WasmEnvironmentManager,
    environment_name: &str,
) -> Result<EntryMetadata> {
    let session = "agent_fetch";
    let _ = env_manager
        .send_command(
            environment_name,
            EnvironmentCommand::StartRepl {
                session: Some(session.to_string()),
            },
        )
        .await?;

    // The snippet reads only metadata so prompts stay small and fast.
    let snippet = r#"
{
    let history_raw = std::fs::read_to_string("../fs/history.json").unwrap_or_default();
    let latest_raw  = std::fs::read_to_string("../fs/latest").unwrap_or_default();

    let version_count = history_raw.matches("\"hash\":").count();
    let bytes = latest_raw.as_bytes().len();
    let chars = latest_raw.chars().count();
    let lines = latest_raw.lines().count();
    let preview: String = latest_raw.lines().next().unwrap_or("").chars().take(120).collect();
    let mtime_secs = std::fs::metadata("../fs/latest")
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0u64);

    println!("AGENT_FETCH_START");
    println!("versions={}", version_count);
    println!("bytes={}", bytes);
    println!("chars={}", chars);
    println!("lines={}", lines);
    println!("mtime_secs={}", mtime_secs);
    println!("preview={:?}", preview);
    println!("AGENT_FETCH_END");
}
"#;

    let eval = env_manager
        .send_command(
            environment_name,
            EnvironmentCommand::ReplEval {
                session: session.to_string(),
                snippet: snippet.to_string(),
            },
        )
        .await?;

    let stdout = eval
        .payload
        .get("stdout")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    parse_agent_fetch_metadata(stdout)
        .ok_or_else(|| eyre!("REPL fetch snippet produced no parseable AGENT_FETCH output"))
}

fn parse_agent_fetch_metadata(stdout: &str) -> Option<EntryMetadata> {
    let start_tag = "AGENT_FETCH_START\n";
    let end_tag   = "\nAGENT_FETCH_END";
    let start = stdout.find(start_tag)? + start_tag.len();
    let end   = stdout[start..].find(end_tag)? + start;
    let section = &stdout[start..end];

    let mut versions:   usize = 0;
    let mut bytes:      usize = 0;
    let mut chars:      usize = 0;
    let mut lines:      usize = 0;
    let mut mtime_secs: u64   = 0;
    let mut preview  = String::new();

    for line in section.lines() {
        if let Some(v) = line.strip_prefix("versions=")     { versions   = v.parse().unwrap_or(0); }
        else if let Some(v) = line.strip_prefix("bytes=")   { bytes      = v.parse().unwrap_or(0); }
        else if let Some(v) = line.strip_prefix("chars=")   { chars      = v.parse().unwrap_or(0); }
        else if let Some(v) = line.strip_prefix("lines=")   { lines      = v.parse().unwrap_or(0); }
        else if let Some(v) = line.strip_prefix("mtime_secs=") { mtime_secs = v.parse().unwrap_or(0); }
        else if let Some(v) = line.strip_prefix("preview=") { preview = serde_json::from_str(v).unwrap_or_default(); }
    }

    let latest_modified_at = chrono::DateTime::from_timestamp(mtime_secs as i64, 0)
        .map(|dt: chrono::DateTime<Utc>| dt.to_rfc3339())
        .unwrap_or_else(|| "unknown".to_string());

    let repl_metadata = format!(
        "bytes={}|chars={}|lines={}|preview={}",
        bytes, chars, lines, preview
    );

    Some(EntryMetadata {
        history_versions: versions,
        latest_bytes: bytes,
        latest_chars: chars,
        latest_lines: lines,
        latest_modified_at,
        repl_metadata,
    })
}

fn parse_repl_read_request(answer: &str) -> Option<String> {
    for line in answer.lines() {
        let trimmed = line.trim();
        if let Some(path) = trimmed.strip_prefix("REPL_READ:") {
            let candidate = path.trim();
            if !candidate.is_empty() {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

async fn read_path_via_repl(
    env_manager: &WasmEnvironmentManager,
    environment_name: &str,
    relative_path: &str,
) -> Result<String> {
    let session = "agent_tool";
    let _ = env_manager
        .send_command(
            environment_name,
            EnvironmentCommand::StartRepl {
                session: Some(session.to_string()),
            },
        )
        .await?;

    let path_literal = format!("{:?}", relative_path);
    let snippet = format!(
        r#"
{{
    let path: &str = {path_literal};
    match std::fs::read_to_string(path) {{
        Ok(content) => {{
            let clipped: String = content.chars().take({max_chars}).collect();
            println!("AGENT_READ_START");
            println!("AGENT_READ_CONTENT={{:?}}", clipped);
            println!("AGENT_READ_END");
        }}
        Err(err) => {{
            println!("AGENT_READ_START");
            println!("AGENT_READ_CONTENT={{:?}}", format!("READ_ERROR: {{}}", err));
            println!("AGENT_READ_END");
        }}
    }}
}}
"#,
        path_literal = path_literal,
        max_chars = MAX_REPL_READ_CHARS,
    );

    let eval = env_manager
        .send_command(
            environment_name,
            EnvironmentCommand::ReplEval {
                session: session.to_string(),
                snippet,
            },
        )
        .await?;

    let stdout = eval
        .payload
        .get("stdout")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let start_tag = "AGENT_READ_START\n";
    let end_tag = "\nAGENT_READ_END";
    let start = stdout
        .find(start_tag)
        .ok_or_else(|| eyre!("missing AGENT_READ_START in REPL output"))?
        + start_tag.len();
    let end = stdout[start..]
        .find(end_tag)
        .ok_or_else(|| eyre!("missing AGENT_READ_END in REPL output"))?
        + start;

    for line in stdout[start..end].lines() {
        if let Some(raw) = line.strip_prefix("AGENT_READ_CONTENT=") {
            return Ok(serde_json::from_str::<String>(raw).unwrap_or_default());
        }
    }

    Err(eyre!("missing AGENT_READ_CONTENT in REPL output"))
}

async fn run_llm_via_url(
    client: &reqwest::Client,
    llm_url: &str,
    api_type: LlmApiType,
    model_name: &str,
    n_predict: u32,
    temperature: f32,
    prompt: &str,
) -> Result<String> {
    let base = llm_url.trim();
    if base.is_empty() {
        bail!("llm_url cannot be empty");
    }

    let base_no_slash = base.trim_end_matches('/');
    match api_type {
        LlmApiType::Llama => run_llama_completion(client, base_no_slash, n_predict, temperature, prompt).await,
        LlmApiType::OpenAi => {
            run_openai_chat_completion(client, base_no_slash, model_name, n_predict, temperature, prompt).await
        }
    }
}

async fn run_llama_completion(
    client: &reqwest::Client,
    base_no_slash: &str,
    n_predict: u32,
    temperature: f32,
    prompt: &str,
) -> Result<String> {
    let completion_url = if base_no_slash.ends_with("/completion") {
        base_no_slash.to_string()
    } else {
        format!("{}/completion", base_no_slash)
    };

    let response = timeout(
        Duration::from_secs(LLM_STEP_TIMEOUT_SECS),
        client
            .post(&completion_url)
            .json(&serde_json::json!({
                "prompt": prompt,
                "n_predict": n_predict,
                "temperature": temperature,
                "stream": false
            }))
            .send(),
    )
    .await
    .map_err(|_| eyre!("LLM URL request timed out after {}s ({})", LLM_STEP_TIMEOUT_SECS, completion_url))?
    .with_context(|| format!("failed to call LLM URL endpoint {}", completion_url))?
    .error_for_status()
    .with_context(|| format!("LLM URL endpoint returned non-success status: {}", completion_url))?;

    let body: Value = response
        .json()
        .await
        .context("failed to parse JSON from llama /completion response")?;

    if let Some(content) = body.get("content").and_then(Value::as_str) {
        return Ok(content.trim().to_string());
    }
    if let Some(content) = body.get("response").and_then(Value::as_str) {
        return Ok(content.trim().to_string());
    }
    if let Some(content) = body
        .get("choices")
        .and_then(|v| v.get(0))
        .and_then(|v| v.get("text"))
        .and_then(Value::as_str)
    {
        return Ok(content.trim().to_string());
    }

    bail!("llama API response JSON did not contain recognizable content fields")
}

async fn run_openai_chat_completion(
    client: &reqwest::Client,
    base_no_slash: &str,
    model_name: &str,
    n_predict: u32,
    temperature: f32,
    prompt: &str,
) -> Result<String> {
    let chat_url = if base_no_slash.ends_with("/v1/chat/completions") {
        base_no_slash.to_string()
    } else {
        format!("{}/v1/chat/completions", base_no_slash)
    };

    let response = timeout(
        Duration::from_secs(LLM_STEP_TIMEOUT_SECS),
        client
            .post(&chat_url)
            .json(&serde_json::json!({
                "model": model_name,
                "messages": [{"role": "user", "content": prompt}],
                "temperature": temperature,
                "max_tokens": n_predict,
                "stream": false
            }))
            .send(),
    )
    .await
    .map_err(|_| eyre!("LLM URL request timed out after {}s ({})", LLM_STEP_TIMEOUT_SECS, chat_url))?
    .with_context(|| format!("failed to call LLM URL endpoint {}", chat_url))?
    .error_for_status()
    .with_context(|| format!("LLM URL endpoint returned non-success status: {}", chat_url))?;

    let body: Value = response
        .json()
        .await
        .context("failed to parse JSON from OpenAI chat response")?;

    if let Some(content) = body
        .get("choices")
        .and_then(|v| v.get(0))
        .and_then(|v| v.get("message"))
        .and_then(|v| v.get("content"))
    {
        if let Some(text) = openai_message_content_to_string(content) {
            return Ok(text);
        }
    }

    if let Some(content) = body
        .get("choices")
        .and_then(|v| v.get(0))
        .and_then(|v| v.get("text"))
        .and_then(Value::as_str)
    {
        return Ok(content.trim().to_string());
    }

    bail!("openai API response JSON did not contain recognizable content fields")
}

fn openai_message_content_to_string(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.trim().to_string());
    }

    let parts = value.as_array()?;
    let mut combined = String::new();
    for part in parts {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(text.trim());
        }
    }

    if combined.is_empty() {
        None
    } else {
        Some(combined)
    }
}
