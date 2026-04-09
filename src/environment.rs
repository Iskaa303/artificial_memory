#![allow(dead_code)]

use chrono::Utc;
use eyre::{Context, Result, bail, eyre};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use tokio::process::Command;
use walkdir::WalkDir;
use wasmtime::{Engine, Instance, Module, Store};

const WASM_COMMAND_BRIDGE_WAT: &str = r#"
(module
    (memory (export "memory") 2)
  (global $heap (mut i32) (i32.const 1024))
  (global $out_ptr (mut i32) (i32.const 0))
  (global $out_len (mut i32) (i32.const 0))

    (func (export "alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr)
  )

    (func (export "handle_command") (param $ptr i32) (param $len i32) (result i32)
    (global.set $out_ptr (local.get $ptr))
    (global.set $out_len (local.get $len))
    (i32.const 0)
  )

    (func (export "output_ptr") (result i32)
    (global.get $out_ptr)
  )

    (func (export "output_len") (result i32)
    (global.get $out_len)
  )
)
"#;

const DEFAULT_SANDBOX_MAIN: &str = "fn main() { println!(\"sandbox ready\"); }\n";

#[derive(Serialize)]
struct SandboxCargoToml {
        package: SandboxPackageSection,
        dependencies: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct SandboxPackageSection {
        name: String,
        version: String,
        edition: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustCrateSpec {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentManifest {
    pub name: String,
    pub created_at: String,
    pub selected_sources: Vec<String>,
    pub fs_root: String,
    pub config_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EnvironmentToml {
    #[serde(default)]
    pub rust: RustConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EnvironmentProfileToml {
    #[serde(default)]
    pub profile: ProfileMetadata,
    #[serde(default)]
    pub rust: RustConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileMetadata {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tier: String,
}

impl Default for ProfileMetadata {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            tier: "standard".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryFolderEnvironmentOption {
    pub folder: String,
    pub selected_profile: String,
    pub available_profiles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustConfig {
    #[serde(default = "default_rust_edition")]
    pub edition: String,
    #[serde(default)]
    pub crates: Vec<RustCrateSpec>,
    #[serde(default)]
    pub prelude: Vec<String>,
}

impl Default for RustConfig {
    fn default() -> Self {
        Self {
            edition: default_rust_edition(),
            crates: Vec::new(),
            prelude: Vec::new(),
        }
    }
}

fn default_rust_edition() -> String {
    "2021".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvironmentCommand {
    ListFiles { under: Option<String> },
    ReadFile { path: String },
    WriteFile { path: String, content: String },
    CargoCheck,
    StartRepl { session: Option<String> },
    ReplEval { session: String, snippet: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentCommandResult {
    pub ok: bool,
    pub message: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ReplSessionState {
    snippets: Vec<String>,
}

pub struct WasmEnvironmentManager {
    memory_root: PathBuf,
    workspace_root: PathBuf,
    engine: Engine,
    command_module: Module,
}

impl WasmEnvironmentManager {
    pub fn new(memory_root: impl Into<PathBuf>, workspace_root: impl Into<PathBuf>) -> Result<Self> {
        let memory_root = memory_root.into();
        let workspace_root = workspace_root.into();

        let engine = Engine::default();
        let wasm = wat::parse_str(WASM_COMMAND_BRIDGE_WAT).context("failed to parse wasm command bridge")?;
        let command_module = Module::new(&engine, wasm)
            .map_err(|e| eyre!("failed to compile wasm command bridge: {e}"))?;

        Ok(Self {
            memory_root,
            workspace_root,
            engine,
            command_module,
        })
    }

    pub async fn ensure_default_environments(
        &self,
        selected_paths: &[PathBuf],
    ) -> Result<Vec<String>> {
        self.ensure_profile_catalog().await?;

        let profiles = self.available_profile_names().await?;
        let mut created = Vec::new();
        for profile in profiles {
            let env_name = format!("default-{}", profile);
            if self.environment_root(&env_name).exists() {
                continue;
            }

            self.create_environment_with_profile(&env_name, selected_paths, &profile)
                .await?;
            created.push(env_name);
        }

        Ok(created)
    }

    pub async fn create_environment_with_profile(
        &self,
        name: &str,
        selected_paths: &[PathBuf],
        profile_name: &str,
    ) -> Result<EnvironmentManifest> {
        self.ensure_profile_catalog().await?;
        let profile_path = self.profile_path(profile_name);
        if !profile_path.exists() {
            bail!("profile does not exist: {profile_name}");
        }

        self.create_environment(name, selected_paths, Some(profile_path.as_path()))
            .await
    }

    pub async fn set_memory_folder_profile(
        &self,
        memory_folder: &str,
        profile_name: &str,
    ) -> Result<()> {
        let _ = (memory_folder, profile_name);
        bail!("memory folder profile mapping is code-defined; update select_profile_for_memory_folder()");
    }

    pub async fn create_or_get_environment_for_memory_folder(
        &self,
        memory_folder: &str,
        selected_paths: &[PathBuf],
    ) -> Result<EnvironmentManifest> {
        self.ensure_profile_catalog().await?;
        let folder = self.normalize_memory_folder(memory_folder)?;
        let profile = self.select_profile_for_memory_folder(&folder);
        let _ = selected_paths;

        self.create_linked_environment_for_memory_folder(
            &folder,
            &format!("default-{}", self.slugify(&profile)),
            &profile,
        )
        .await
    }

    pub async fn create_environment_for_memory_entry(
        &self,
        memory_folder: &str,
    ) -> Result<EnvironmentManifest> {
        let folder = self.normalize_memory_folder(memory_folder)?;
        let entry_path = self.memory_root.join(&folder);
        if !entry_path.is_dir() {
            bail!("memory entry does not exist: {}", entry_path.display());
        }

        let _ = entry_path;
        self.create_or_get_environment_for_memory_folder(&folder, &[]).await
    }

    pub async fn create_new_environment_for_memory_entry(
        &self,
        memory_folder: &str,
        environment_label: &str,
        profile_name: Option<&str>,
    ) -> Result<EnvironmentManifest> {
        self.ensure_profile_catalog().await?;

        let folder = self.normalize_memory_folder(memory_folder)?;
        let profile = profile_name
            .map(|value| self.normalize_profile_name(value))
            .unwrap_or_else(|| self.select_profile_for_memory_folder(&folder));

        self.create_linked_environment_for_memory_folder(&folder, environment_label, &profile)
            .await
    }

    pub async fn create_environments_for_memory_entries(
        &self,
    ) -> Result<Vec<EnvironmentManifest>> {
        let folders = self.list_memory_entries().await?;
        let mut manifests = Vec::new();
        for folder in folders {
            manifests.push(self.create_environment_for_memory_entry(&folder).await?);
        }

        Ok(manifests)
    }

    pub async fn list_memory_folder_environment_options(
        &self,
    ) -> Result<Vec<MemoryFolderEnvironmentOption>> {
        self.ensure_profile_catalog().await?;
        let profiles = self.available_profile_names().await?;
        let mut options = Vec::new();
        let folders = self.list_memory_entries().await?;

        for folder in folders {
            options.push(MemoryFolderEnvironmentOption {
                selected_profile: self.select_profile_for_memory_folder(&folder),
                folder,
                available_profiles: profiles.clone(),
            });
        }

        Ok(options)
    }

    async fn ensure_profile_catalog(&self) -> Result<()> {
        let profiles_root = self.profiles_root();
        tokio::fs::create_dir_all(&profiles_root)
            .await
            .with_context(|| format!("failed to create {}", profiles_root.display()))?;

        self.write_default_profile_if_missing(
            "minimal-sandbox",
            EnvironmentProfileToml {
                profile: ProfileMetadata {
                    name: "minimal-sandbox".to_string(),
                    description: "Minimal isolated Rust profile with ergonomic error handling.".to_string(),
                    tier: "strict".to_string(),
                },
                rust: RustConfig {
                    edition: "2021".to_string(),
                    crates: vec![RustCrateSpec {
                        name: "eyre".to_string(),
                        version: "0.6".to_string(),
                    }],
                    prelude: vec![
                        "use eyre::{Result, WrapErr};".to_string(),
                        "use std::path::{Path, PathBuf};".to_string(),
                    ],
                },
            },
        )
        .await?;

        self.write_default_profile_if_missing(
            "rust-core",
            EnvironmentProfileToml {
                profile: ProfileMetadata {
                    name: "rust-core".to_string(),
                    description: "Core Rust profile for REPL usage, serialization, and small calculations."
                        .to_string(),
                    tier: "standard".to_string(),
                },
                rust: RustConfig {
                    edition: "2021".to_string(),
                    crates: vec![
                        RustCrateSpec {
                            name: "eyre".to_string(),
                            version: "0.6".to_string(),
                        },
                        RustCrateSpec {
                            name: "serde".to_string(),
                            version: "1.0".to_string(),
                        },
                        RustCrateSpec {
                            name: "serde_json".to_string(),
                            version: "1.0".to_string(),
                        },
                        RustCrateSpec {
                            name: "num-traits".to_string(),
                            version: "0.2".to_string(),
                        },
                    ],
                    prelude: vec![
                        "use eyre::{Result, WrapErr};".to_string(),
                        "use serde::{Deserialize, Serialize};".to_string(),
                        "use num_traits::{Float, ToPrimitive};".to_string(),
                    ],
                },
            },
        )
        .await?;

        self.write_default_profile_if_missing(
            "file-operations",
            EnvironmentProfileToml {
                profile: ProfileMetadata {
                    name: "file-operations".to_string(),
                    description:
                        "File-manipulation profile with filesystem helpers, globs, walking, and convenience snippets."
                            .to_string(),
                    tier: "extended".to_string(),
                },
                rust: RustConfig {
                    edition: "2021".to_string(),
                    crates: vec![
                        RustCrateSpec {
                            name: "eyre".to_string(),
                            version: "0.6".to_string(),
                        },
                        RustCrateSpec {
                            name: "walkdir".to_string(),
                            version: "2.5".to_string(),
                        },
                        RustCrateSpec {
                            name: "glob".to_string(),
                            version: "0.3".to_string(),
                        },
                        RustCrateSpec {
                            name: "fs-err".to_string(),
                            version: "3.1".to_string(),
                        },
                        RustCrateSpec {
                            name: "num-traits".to_string(),
                            version: "0.2".to_string(),
                        },
                    ],
                    prelude: vec![
                        "use eyre::{Result, WrapErr};".to_string(),
                        "use fs_err as fs;".to_string(),
                        "use glob::glob;".to_string(),
                        "use std::path::PathBuf;".to_string(),
                        "use walkdir::WalkDir;".to_string(),
                        "fn read_text(path: impl AsRef<std::path::Path>) -> Result<String> { fs::read_to_string(path.as_ref()).wrap_err(\"failed to read text file\") }".to_string(),
                        "fn write_text(path: impl AsRef<std::path::Path>, content: &str) -> Result<()> { fs::write(path.as_ref(), content).wrap_err(\"failed to write text file\")?; Ok(()) }".to_string(),
                        "fn list_files(root: impl AsRef<std::path::Path>) -> Vec<PathBuf> { WalkDir::new(root).into_iter().filter_map(|entry| entry.ok()).filter(|entry| entry.file_type().is_file()).map(|entry| entry.path().to_path_buf()).collect() }".to_string(),
                    ],
                },
            },
        )
        .await?;

        self.write_default_profile_if_missing(
            "archive-operations",
            EnvironmentProfileToml {
                profile: ProfileMetadata {
                    name: "archive-operations".to_string(),
                    description: "Archive and compression profile for unzipping and inspecting packaged files.".to_string(),
                    tier: "extended".to_string(),
                },
                rust: RustConfig {
                    edition: "2021".to_string(),
                    crates: vec![
                        RustCrateSpec {
                            name: "eyre".to_string(),
                            version: "0.6".to_string(),
                        },
                        RustCrateSpec {
                            name: "zip".to_string(),
                            version: "2.2".to_string(),
                        },
                        RustCrateSpec {
                            name: "flate2".to_string(),
                            version: "1.0".to_string(),
                        },
                    ],
                    prelude: vec![
                        "use eyre::{Result, WrapErr};".to_string(),
                        "use std::fs::File;".to_string(),
                        "use std::io::{copy, Read};".to_string(),
                        "use std::path::Path;".to_string(),
                        "use zip::ZipArchive;".to_string(),
                        "fn open_zip(path: impl AsRef<Path>) -> Result<ZipArchive<File>> { let file = File::open(path.as_ref()).wrap_err(\"failed to open zip file\")?; ZipArchive::new(file).wrap_err(\"failed to parse zip archive\") }".to_string(),
                    ],
                },
            },
        )
        .await?;

        Ok(())
    }

    async fn write_default_profile_if_missing(
        &self,
        profile_name: &str,
        profile: EnvironmentProfileToml,
    ) -> Result<()> {
        let path = self.profile_path(profile_name);
        if path.exists() {
            return Ok(());
        }

        let raw = toml::to_string_pretty(&profile).context("failed to render profile toml")?;
        tokio::fs::write(&path, raw)
            .await
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    async fn available_profile_names(&self) -> Result<Vec<String>> {
        let mut names = BTreeSet::new();
        let mut dir = tokio::fs::read_dir(self.profiles_root())
            .await
            .context("failed to read profile directory")?;

        while let Some(entry) = dir.next_entry().await? {
            if entry.file_type().await?.is_file() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("toml") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        names.insert(stem.to_string());
                    }
                }
            }
        }

        Ok(names.into_iter().collect())
    }

    fn config_root(&self) -> PathBuf {
        self.workspace_root.join("config").join("environment")
    }

    fn profiles_root(&self) -> PathBuf {
        self.config_root().join("profiles")
    }

    fn profile_path(&self, profile_name: &str) -> PathBuf {
        self.profiles_root()
            .join(format!("{}.toml", self.normalize_profile_name(profile_name)))
    }

    fn normalize_profile_name(&self, profile_name: &str) -> String {
        profile_name.trim().to_lowercase().replace('_', "-")
    }

    fn normalize_memory_folder(&self, folder: &str) -> Result<String> {
        let trimmed = folder.trim();
        if trimmed.is_empty() {
            bail!("memory folder cannot be empty");
        }

        let rel = Path::new(trimmed);
        if rel.is_absolute() {
            bail!("memory folder must be relative: {trimmed}");
        }

        if rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            bail!("path traversal is not allowed for memory folder: {trimmed}");
        }

        Ok(trimmed.replace('\\', "/"))
    }

    fn select_profile_for_memory_folder(&self, folder: &str) -> String {
        let normalized = folder.to_ascii_lowercase();
        match normalized.as_str() {
            "src_main.rs" => "rust-core".to_string(),
            "src_process.rs" | "src_storage.rs" => "file-operations".to_string(),
            "tests_test.txt" | "tests_test2.txt" => "minimal-sandbox".to_string(),
            _ if normalized.starts_with("tests_") => "minimal-sandbox".to_string(),
            _ if normalized.starts_with("src_") => "rust-core".to_string(),
            _ => "rust-core".to_string(),
        }
    }

    async fn list_memory_entries(&self) -> Result<Vec<String>> {
        if !self.memory_root.exists() {
            tokio::fs::create_dir_all(&self.memory_root)
                .await
                .with_context(|| format!("failed to create {}", self.memory_root.display()))?;
            return Ok(Vec::new());
        }

        let mut folders = Vec::new();
        let mut dir = tokio::fs::read_dir(&self.memory_root)
            .await
            .context("failed to list memory root")?;
        while let Some(entry) = dir.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                let folder_name = entry.file_name().to_string_lossy().to_string();
                if folder_name == "environment" {
                    continue;
                }
                folders.push(folder_name);
            }
        }

        folders.sort();
        Ok(folders)
    }

    fn slugify(&self, value: &str) -> String {
        let mut out = String::new();
        for ch in value.chars() {
            if ch.is_ascii_alphanumeric() {
                out.push(ch.to_ascii_lowercase());
            } else {
                out.push('-');
            }
        }
        while out.contains("--") {
            out = out.replace("--", "-");
        }
        out.trim_matches('-').to_string()
    }

    pub async fn create_environment(
        &self,
        name: &str,
        selected_paths: &[PathBuf],
        config_source: Option<&Path>,
    ) -> Result<EnvironmentManifest> {
        if name.trim().is_empty() {
            bail!("environment name cannot be empty");
        }

        let env_root = self.environment_root(name);
        if env_root.exists() {
            bail!("environment already exists: {name}");
        }

        let fs_root = self.create_environment_fs_root(&env_root).await?;
        let source_labels = self
            .materialize_environment_sources(&fs_root, selected_paths)
            .await?;

        let config_path = env_root.join("Environment.toml");
        self.initialize_environment_toml(&config_path, config_source).await?;

        let manifest = EnvironmentManifest {
            name: name.to_string(),
            created_at: Utc::now().to_rfc3339(),
            selected_sources: source_labels,
            fs_root: fs_root.to_string_lossy().to_string(),
            config_path: config_path.to_string_lossy().to_string(),
        };

        self.write_manifest(name, &manifest).await?;
        self.prepare_environment_runtime(name).await?;

        Ok(manifest)
    }

    pub async fn send_command(
        &self,
        environment_name: &str,
        command: EnvironmentCommand,
    ) -> Result<EnvironmentCommandResult> {
        let command_json = serde_json::to_string(&command).context("failed to encode command")?;
        let bridged_json = self.wasm_bridge_roundtrip(&command_json)?;
        let bridged_command: EnvironmentCommand =
            serde_json::from_str(&bridged_json).context("wasm bridge returned invalid command")?;

        let result = self.execute_command(environment_name, bridged_command).await?;
        self.append_command_log(environment_name, &command, &result).await?;

        Ok(result)
    }

    pub async fn create_repl_session(&self, environment_name: &str, session: &str) -> Result<()> {
        let session_path = self.repl_session_path(environment_name, session);
        if let Some(parent) = session_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        if !session_path.exists() {
            let data = serde_json::to_string_pretty(&ReplSessionState::default())?;
            tokio::fs::write(&session_path, data)
                .await
                .with_context(|| format!("failed to write {}", session_path.display()))?;
        }

        Ok(())
    }

    fn wasm_bridge_roundtrip(&self, input_json: &str) -> Result<String> {
        let mut store = Store::new(&self.engine, ());
        let instance = Instance::new(&mut store, &self.command_module, &[])
            .map_err(|e| eyre!("failed to instantiate wasm bridge: {e}"))?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| eyre!("wasm bridge memory export missing"))?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|e| eyre!("wasm bridge alloc export missing: {e}"))?;
        let handle = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "handle_command")
            .map_err(|e| eyre!("wasm bridge handle_command export missing: {e}"))?;
        let output_ptr = instance
            .get_typed_func::<(), i32>(&mut store, "output_ptr")
            .map_err(|e| eyre!("wasm bridge output_ptr export missing: {e}"))?;
        let output_len = instance
            .get_typed_func::<(), i32>(&mut store, "output_len")
            .map_err(|e| eyre!("wasm bridge output_len export missing: {e}"))?;

        let input_bytes = input_json.as_bytes();
        let ptr = alloc
            .call(&mut store, input_bytes.len() as i32)
            .map_err(|e| eyre!("wasm bridge alloc failed: {e}"))?;

        memory
            .write(&mut store, ptr as usize, input_bytes)
            .context("failed to write command into wasm memory")?;

        let rc = handle
            .call(&mut store, (ptr, input_bytes.len() as i32))
            .map_err(|e| eyre!("wasm bridge execution failed: {e}"))?;
        if rc != 0 {
            bail!("wasm bridge returned non-zero status: {rc}");
        }

        let out_ptr = output_ptr
            .call(&mut store, ())
            .map_err(|e| eyre!("wasm bridge output_ptr failed: {e}"))?;
        let out_len = output_len
            .call(&mut store, ())
            .map_err(|e| eyre!("wasm bridge output_len failed: {e}"))?;

        let mut out = vec![0u8; out_len as usize];
        memory
            .read(&mut store, out_ptr as usize, &mut out)
            .context("failed to read command from wasm memory")?;

        String::from_utf8(out).context("wasm bridge output is not utf-8")
    }

    async fn execute_command(
        &self,
        environment_name: &str,
        command: EnvironmentCommand,
    ) -> Result<EnvironmentCommandResult> {
        let fs_root = self.environment_root(environment_name).join("fs");

        match command {
            EnvironmentCommand::ListFiles { under } => {
                let root = if let Some(path) = under {
                    self.safe_join(&fs_root, &path)?
                } else {
                    fs_root.clone()
                };

                if !root.exists() {
                    bail!("requested path does not exist: {}", root.display());
                }

                let mut files = Vec::new();
                for entry in WalkDir::new(&root).min_depth(1).max_depth(64) {
                    let entry = entry.context("failed while walking files")?;
                    let rel = entry
                        .path()
                        .strip_prefix(&fs_root)
                        .unwrap_or(entry.path())
                        .to_string_lossy()
                        .replace('\\', "/");
                    files.push(rel);
                }

                files.sort();
                Ok(EnvironmentCommandResult {
                    ok: true,
                    message: "listed files".to_string(),
                    payload: json!({ "entries": files }),
                })
            }
            EnvironmentCommand::ReadFile { path } => {
                let file_path = self.safe_join(&fs_root, &path)?;
                let content = tokio::fs::read_to_string(&file_path)
                    .await
                    .with_context(|| format!("failed to read {}", file_path.display()))?;

                Ok(EnvironmentCommandResult {
                    ok: true,
                    message: "read file".to_string(),
                    payload: json!({ "path": path, "content": content }),
                })
            }
            EnvironmentCommand::WriteFile { path, content } => {
                let file_path = self.safe_join(&fs_root, &path)?;
                if let Some(parent) = file_path.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .context("failed to create parent directories")?;
                }
                tokio::fs::write(&file_path, content)
                    .await
                    .with_context(|| format!("failed to write {}", file_path.display()))?;

                Ok(EnvironmentCommandResult {
                    ok: true,
                    message: "wrote file".to_string(),
                    payload: json!({ "path": path }),
                })
            }
            EnvironmentCommand::CargoCheck => {
                self.ensure_rust_project(environment_name).await?;
                let output = self.run_cargo(environment_name, &["check", "--quiet"]).await?;

                Ok(EnvironmentCommandResult {
                    ok: output.status.success(),
                    message: "cargo check completed".to_string(),
                    payload: json!({
                        "exit_code": output.status.code(),
                        "stdout": String::from_utf8_lossy(&output.stdout),
                        "stderr": String::from_utf8_lossy(&output.stderr)
                    }),
                })
            }
            EnvironmentCommand::StartRepl { session } => {
                self.ensure_rust_project(environment_name).await?;
                let session_name = session.unwrap_or_else(|| "default".to_string());
                self.create_repl_session(environment_name, &session_name).await?;

                Ok(EnvironmentCommandResult {
                    ok: true,
                    message: "repl session ready".to_string(),
                    payload: json!({
                        "session": session_name,
                        "prompt": "rust> "
                    }),
                })
            }
            EnvironmentCommand::ReplEval { session, snippet } => {
                self.ensure_rust_project(environment_name).await?;
                self.create_repl_session(environment_name, &session).await?;
                self.validate_repl_snippet(&snippet)?;

                let mut state = self.read_repl_state(environment_name, &session).await?;
                let mut trial = state.clone();
                trial.snippets.push(snippet.clone());

                let source = self.build_repl_main(environment_name, &trial).await?;
                self.write_rust_main(environment_name, &source).await?;
                let output = self
                    .run_cargo(environment_name, &["run", "--quiet", "--"])
                    .await?;

                let ok = output.status.success();
                if ok {
                    state.snippets.push(snippet);
                    self.write_repl_state(environment_name, &session, &state).await?;
                }

                Ok(EnvironmentCommandResult {
                    ok,
                    message: if ok {
                        "repl eval succeeded".to_string()
                    } else {
                        "repl eval failed".to_string()
                    },
                    payload: json!({
                        "session": session,
                        "exit_code": output.status.code(),
                        "stdout": String::from_utf8_lossy(&output.stdout),
                        "stderr": String::from_utf8_lossy(&output.stderr)
                    }),
                })
            }
        }
    }

    async fn append_command_log(
        &self,
        environment_name: &str,
        command: &EnvironmentCommand,
        result: &EnvironmentCommandResult,
    ) -> Result<()> {
        let env_root = self.environment_root(environment_name);
        let log_path = env_root.join("command-log.jsonl");

        let line = serde_json::to_string(&json!({
            "at": Utc::now().to_rfc3339(),
            "command": command,
            "result": result
        }))?;

        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await
            .with_context(|| format!("failed to open {}", log_path.display()))?;

        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;

        Ok(())
    }

    async fn write_manifest(&self, name: &str, manifest: &EnvironmentManifest) -> Result<()> {
        let env_root = self.environment_root(name);
        tokio::fs::create_dir_all(&env_root)
            .await
            .context("failed to create environment root")?;
        let path = env_root.join("manifest.json");
        let data = serde_json::to_string_pretty(manifest)?;
        tokio::fs::write(&path, data)
            .await
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    async fn initialize_environment_toml(
        &self,
        target_path: &Path,
        config_source: Option<&Path>,
    ) -> Result<()> {
        if let Some(source_path) = config_source {
            let resolved = self.resolve_source(source_path)?;
            if let Some(parent) = target_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::copy(&resolved, target_path)
                .await
                .with_context(|| format!("failed to copy {}", resolved.display()))?;
            return Ok(());
        }

        let default_cfg = EnvironmentToml::default();
        let toml_data = toml::to_string_pretty(&default_cfg)
            .context("failed to render default Environment.toml")?;
        tokio::fs::write(target_path, toml_data)
            .await
            .with_context(|| format!("failed to write {}", target_path.display()))?;

        Ok(())
    }

    async fn read_manifest(&self, name: &str) -> Result<EnvironmentManifest> {
        let path = self.environment_root(name).join("manifest.json");
        let raw = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&raw).context("failed to parse manifest")
    }

    async fn read_environment_config(&self, environment_name: &str) -> Result<EnvironmentToml> {
        let manifest = self.read_manifest(environment_name).await?;
        let raw = tokio::fs::read_to_string(&manifest.config_path)
            .await
            .with_context(|| format!("failed to read {}", manifest.config_path))?;

        toml::from_str(&raw).context("failed to parse Environment.toml")
    }

    async fn ensure_rust_project(&self, environment_name: &str) -> Result<()> {
        let cfg = self.read_environment_config(environment_name).await?;
        let project_root = self.rust_project_root(environment_name);
        let src_root = project_root.join("src");

        tokio::fs::create_dir_all(&src_root)
            .await
            .context("failed to create rust project src")?;

        let cargo = self.render_sandbox_cargo_toml(&cfg.rust)?;

        tokio::fs::write(project_root.join("Cargo.toml"), cargo)
            .await
            .context("failed to write sandbox Cargo.toml")?;

        let main_file = src_root.join("main.rs");
        if !main_file.exists() {
            tokio::fs::write(&main_file, DEFAULT_SANDBOX_MAIN)
                .await
                .context("failed to write default sandbox main.rs")?;
        }

        let cargo_home = project_root.join(".cargo-home");
        tokio::fs::create_dir_all(cargo_home)
            .await
            .context("failed to create isolated cargo home")?;

        Ok(())
    }

    async fn build_repl_main(
        &self,
        environment_name: &str,
        state: &ReplSessionState,
    ) -> Result<String> {
        let cfg = self.read_environment_config(environment_name).await?;

        let mut source = String::new();
        for line in &cfg.rust.prelude {
            source.push_str(line);
            source.push('\n');
        }

        source.push_str("fn main() {\n");
        for snippet in &state.snippets {
            for line in snippet.lines() {
                source.push_str("    ");
                source.push_str(line);
                source.push('\n');
            }
        }
        source.push_str("}\n");

        Ok(source)
    }

    async fn write_rust_main(&self, environment_name: &str, main_rs: &str) -> Result<()> {
        let project_root = self.rust_project_root(environment_name);
        tokio::fs::create_dir_all(project_root.join("src")).await?;
        tokio::fs::write(project_root.join("src/main.rs"), main_rs)
            .await
            .context("failed to write sandbox main.rs")?;
        Ok(())
    }

    async fn read_repl_state(&self, environment_name: &str, session: &str) -> Result<ReplSessionState> {
        let path = self.repl_session_path(environment_name, session);
        let raw = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&raw).context("failed to parse repl state")
    }

    async fn write_repl_state(
        &self,
        environment_name: &str,
        session: &str,
        state: &ReplSessionState,
    ) -> Result<()> {
        let path = self.repl_session_path(environment_name, session);
        let raw = serde_json::to_string_pretty(state)?;
        tokio::fs::write(&path, raw)
            .await
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    async fn run_cargo(&self, environment_name: &str, args: &[&str]) -> Result<std::process::Output> {
        use std::time::Duration;
        use tokio::time::timeout;

        let project_root = self.rust_project_root(environment_name);
        let cargo_home = project_root.join(".cargo-home");
        let target_dir = project_root.join("target");

        let mut cmd = Command::new("cargo");
        cmd.args(args)
            .current_dir(&project_root)
            .env("CARGO_HOME", cargo_home)
            .env("CARGO_TARGET_DIR", target_dir)
            .env_remove("RUSTFLAGS")
            .env_remove("RUSTC_WRAPPER")
            .kill_on_drop(true);

        let output = timeout(Duration::from_secs(45), cmd.output())
            .await
            .context("sandbox cargo command timed out")?
            .context("failed to run cargo in isolated environment")?;

        Ok(output)
    }

    async fn create_environment_fs_root(&self, env_root: &Path) -> Result<PathBuf> {
        let fs_root = env_root.join("fs");
        tokio::fs::create_dir_all(&fs_root)
            .await
            .context("failed to create environment fs root")?;
        Ok(fs_root)
    }

    async fn create_linked_environment_for_memory_folder(
        &self,
        memory_folder: &str,
        environment_label: &str,
        profile_name: &str,
    ) -> Result<EnvironmentManifest> {
        let normalized_folder = self.normalize_memory_folder(memory_folder)?;
        let normalized_profile = self.normalize_profile_name(profile_name);
        let env_name = self.environment_name_for_folder(&normalized_folder, environment_label);
        let env_root = self.memory_environment_root(&normalized_folder, &env_name);
        let entry_root = self.memory_root.join(&normalized_folder);

        let profile_path = self.profile_path(&normalized_profile);
        if !profile_path.exists() {
            bail!("profile does not exist: {normalized_profile}");
        }

        if env_root.exists() {
            return self
                .repair_existing_environment(
                    &env_name,
                    &entry_root,
                    &env_root,
                    profile_path.as_path(),
                )
                .await;
        }

        let fs_root = self.create_environment_fs_root(&env_root).await?;
        self.migrate_memory_entry_contents_to_fs(&entry_root, &fs_root)
            .await?;

        let config_path = env_root.join("Environment.toml");
        self.initialize_environment_toml(&config_path, Some(profile_path.as_path()))
            .await?;

        let manifest = EnvironmentManifest {
            name: env_name,
            created_at: Utc::now().to_rfc3339(),
            selected_sources: vec![entry_root.to_string_lossy().to_string()],
            fs_root: fs_root.to_string_lossy().to_string(),
            config_path: config_path.to_string_lossy().to_string(),
        };

        self.write_manifest(&manifest.name, &manifest).await?;
        self.prepare_environment_runtime(&manifest.name).await?;

        Ok(manifest)
    }

    async fn repair_existing_environment(
        &self,
        env_name: &str,
        entry_root: &Path,
        env_root: &Path,
        profile_path: &Path,
    ) -> Result<EnvironmentManifest> {
        let fs_root = self.create_environment_fs_root(env_root).await?;
        self.migrate_memory_entry_contents_to_fs(entry_root, &fs_root)
            .await?;

        let config_path = env_root.join("Environment.toml");
        if !config_path.exists() {
            self.initialize_environment_toml(&config_path, Some(profile_path))
                .await?;
        }

        let manifest = match self.read_manifest(env_name).await {
            Ok(existing) => existing,
            Err(_) => EnvironmentManifest {
                name: env_name.to_string(),
                created_at: Utc::now().to_rfc3339(),
                selected_sources: vec![entry_root.to_string_lossy().to_string()],
                fs_root: fs_root.to_string_lossy().to_string(),
                config_path: config_path.to_string_lossy().to_string(),
            },
        };

        self.write_manifest(env_name, &manifest).await?;
        self.prepare_environment_runtime(env_name).await?;
        Ok(manifest)
    }

    async fn materialize_environment_sources(
        &self,
        fs_root: &Path,
        selected_paths: &[PathBuf],
    ) -> Result<Vec<String>> {
        let mut source_labels = Vec::new();

        for raw in selected_paths {
            let source = self.resolve_source(raw)?;
            self.copy_source_into_environment(fs_root, &source).await?;
            source_labels.push(source.to_string_lossy().to_string());
        }

        Ok(source_labels)
    }

    async fn copy_source_into_environment(&self, fs_root: &Path, source: &Path) -> Result<()> {
        let rel_target = self.relative_copy_target(source);
        let destination = fs_root.join(rel_target);

        if source.is_dir() {
            return self.copy_dir(source, &destination).await;
        }

        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("failed to create parent destination")?;
        }
        tokio::fs::copy(source, &destination)
            .await
            .with_context(|| format!("failed to copy {}", source.display()))?;

        Ok(())
    }

    async fn migrate_memory_entry_contents_to_fs(
        &self,
        entry_root: &Path,
        fs_root: &Path,
    ) -> Result<()> {
        let mut dir = tokio::fs::read_dir(entry_root)
            .await
            .with_context(|| format!("failed to read {}", entry_root.display()))?;

        while let Some(entry) = dir.next_entry().await? {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if name == "environment"
                || name == "fs"
                || name == "rust"
                || name == "repl"
                || name == "manifest.json"
                || name == "Environment.toml"
                || name == "command-log.jsonl"
            {
                continue;
            }

            let source = entry.path();
            let destination = fs_root.join(&file_name);
            self.move_without_duplicates(&source, &destination).await?;
        }

        Ok(())
    }

    async fn prepare_environment_runtime(&self, environment_name: &str) -> Result<()> {
        self.ensure_rust_project(environment_name).await?;
        self.create_repl_session(environment_name, "default").await?;
        Ok(())
    }

    async fn move_without_duplicates(&self, source: &Path, destination: &Path) -> Result<()> {
        let source_meta = tokio::fs::symlink_metadata(source)
            .await
            .with_context(|| format!("failed to inspect {}", source.display()))?;

        if source_meta.file_type().is_symlink() {
            tokio::fs::remove_file(source)
                .await
                .with_context(|| format!("failed to remove legacy symlink {}", source.display()))?;
            return Ok(());
        }

        if source_meta.is_dir() {
            tokio::fs::create_dir_all(destination)
                .await
                .with_context(|| format!("failed to create {}", destination.display()))?;

            let mut dir = tokio::fs::read_dir(source)
                .await
                .with_context(|| format!("failed to read {}", source.display()))?;

            while let Some(entry) = dir.next_entry().await? {
                let src_child = entry.path();
                let dst_child = destination.join(entry.file_name());
                Box::pin(self.move_without_duplicates(&src_child, &dst_child)).await?;
            }

            tokio::fs::remove_dir(source)
                .await
                .with_context(|| format!("failed to remove {}", source.display()))?;
            return Ok(());
        }

        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        if destination.exists() {
            let dst_meta = tokio::fs::symlink_metadata(destination)
                .await
                .with_context(|| format!("failed to inspect {}", destination.display()))?;

            if dst_meta.file_type().is_symlink() {
                tokio::fs::remove_file(destination).await.with_context(|| {
                    format!("failed to remove legacy symlink {}", destination.display())
                })?;
                tokio::fs::rename(source, destination)
                    .await
                    .with_context(|| format!("failed to move {}", source.display()))?;
                return Ok(());
            }

            let source_bytes = tokio::fs::read(source)
                .await
                .with_context(|| format!("failed to read {}", source.display()))?;
            let destination_bytes = tokio::fs::read(destination)
                .await
                .with_context(|| format!("failed to read {}", destination.display()))?;

            if source_bytes == destination_bytes {
                tokio::fs::remove_file(source)
                    .await
                    .with_context(|| format!("failed to remove duplicate {}", source.display()))?;
                return Ok(());
            }

            tokio::fs::remove_file(destination)
                .await
                .with_context(|| format!("failed to replace {}", destination.display()))?;
        }

        tokio::fs::rename(source, destination)
            .await
            .with_context(|| format!("failed to move {}", source.display()))?;

        Ok(())
    }

    fn render_sandbox_cargo_toml(&self, rust: &RustConfig) -> Result<String> {
        let dependencies = rust
            .crates
            .iter()
            .map(|krate| (krate.name.clone(), krate.version.clone()))
            .collect::<BTreeMap<_, _>>();

        let manifest = SandboxCargoToml {
            package: SandboxPackageSection {
                name: "sandbox_exec".to_string(),
                version: "0.1.0".to_string(),
                edition: rust.edition.clone(),
            },
            dependencies,
        };

        toml::to_string_pretty(&manifest).context("failed to render sandbox Cargo.toml")
    }

    fn environment_name_for_folder(&self, folder: &str, environment_label: &str) -> String {
        let _ = environment_label;
        folder.to_string()
    }

    fn repl_session_path(&self, environment_name: &str, session: &str) -> PathBuf {
        self.environment_root(environment_name)
            .join("repl")
            .join(format!("{}.json", session))
    }

    fn rust_project_root(&self, environment_name: &str) -> PathBuf {
        self.environment_root(environment_name).join("rust")
    }

    fn environment_root(&self, name: &str) -> PathBuf {
        if let Some(found) = self.find_environment_root(name) {
            return found;
        }

        self.shared_environment_root(name)
    }

    fn find_environment_root(&self, name: &str) -> Option<PathBuf> {
        let direct = self.memory_root.join(name);
        if direct.is_dir() {
            return Some(direct);
        }

        let entries = std::fs::read_dir(&self.memory_root).ok()?;
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if !entry_path.is_dir() {
                continue;
            }

            // Legacy nested layout support.
            let local_env = entry_path.join("environment").join(name);
            if local_env.exists() {
                return Some(local_env);
            }

            // Flattened layout support by matching manifest name.
            let manifest = entry_path.join("manifest.json");
            if manifest.is_file() {
                if let Ok(raw) = std::fs::read_to_string(&manifest) {
                    if let Ok(parsed) = serde_json::from_str::<EnvironmentManifest>(&raw) {
                        if parsed.name == name {
                            return Some(entry_path);
                        }
                    }
                }
            }
        }

        None
    }

    fn memory_environment_root(&self, memory_folder: &str, environment_name: &str) -> PathBuf {
        let _ = environment_name;
        self.memory_root.join(memory_folder)
    }

    fn shared_environment_root(&self, environment_name: &str) -> PathBuf {
        self.workspace_root
            .join(".ouroboros")
            .join("environment")
            .join(environment_name)
    }

    fn resolve_source(&self, source: &Path) -> Result<PathBuf> {
        let candidate = if source.is_absolute() {
            source.to_path_buf()
        } else {
            self.workspace_root.join(source)
        };

        let canonical = std::fs::canonicalize(&candidate)
            .with_context(|| format!("selected source not found: {}", candidate.display()))?;

        let workspace_root = std::fs::canonicalize(&self.workspace_root)
            .with_context(|| format!("failed to canonicalize {}", self.workspace_root.display()))?;
        let memory_root = std::fs::canonicalize(&self.memory_root)
            .with_context(|| format!("failed to canonicalize {}", self.memory_root.display()))?;

        if !canonical.starts_with(&workspace_root) && !canonical.starts_with(&memory_root) {
            bail!(
                "selected source must be inside workspace or memory roots: {}",
                canonical.display()
            );
        }

        Ok(canonical)
    }

    fn relative_copy_target(&self, source: &Path) -> PathBuf {
        if let Ok(rel) = source.strip_prefix(&self.workspace_root) {
            rel.to_path_buf()
        } else {
            source
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("external"))
        }
    }

    async fn copy_dir(&self, source: &Path, target: &Path) -> Result<()> {
        tokio::fs::create_dir_all(target)
            .await
            .with_context(|| format!("failed to create {}", target.display()))?;

        for entry in WalkDir::new(source) {
            let entry = entry.context("failed while walking source directory")?;
            let src_path = entry.path();
            let rel = src_path
                .strip_prefix(source)
                .context("failed to compute relative source path")?;
            let dst_path = target.join(rel);

            if entry.file_type().is_dir() {
                tokio::fs::create_dir_all(&dst_path)
                    .await
                    .with_context(|| format!("failed to create {}", dst_path.display()))?;
            } else if entry.file_type().is_file() {
                if let Some(parent) = dst_path.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .with_context(|| format!("failed to create {}", parent.display()))?;
                }
                tokio::fs::copy(src_path, &dst_path)
                    .await
                    .with_context(|| format!("failed to copy {}", src_path.display()))?;
            }
        }

        Ok(())
    }

    fn safe_join(&self, root: &Path, user_path: &str) -> Result<PathBuf> {
        let rel = Path::new(user_path);
        if rel.is_absolute() {
            bail!("absolute paths are not allowed in environment commands");
        }

        if rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            bail!("path traversal is not allowed: {user_path}");
        }

        Ok(root.join(rel))
    }

    fn validate_repl_snippet(&self, snippet: &str) -> Result<()> {
        let lowered = snippet.to_ascii_lowercase();
        let blocked_patterns = [
            "unsafe",
            "std::process",
            "command::new",
            "std::net",
            "tokio::process",
            "libc",
            "include_str!",
            "include_bytes!",
        ];

        for pattern in blocked_patterns {
            if lowered.contains(pattern) {
                bail!("repl snippet contains blocked pattern: {pattern}");
            }
        }

        Ok(())
    }
}
