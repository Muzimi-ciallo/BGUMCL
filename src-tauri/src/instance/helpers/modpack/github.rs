use serde::{Deserialize, Serialize};
use sjmcl_types::error::{BGUMCLError, BGUMCLResult};
use sjmcl_types::storage::{load_json_async, save_json_async};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use tauri_plugin_http::reqwest;

use crate::instance::helpers::misc::get_instance_subdir_path_by_id;
use crate::instance::models::misc::{Instance, InstanceSubdirType};
use crate::tasks::commands::schedule_progressive_task_group;
use crate::tasks::download::DownloadParam;
use crate::tasks::monitor::TaskMonitor;
use crate::tasks::PTaskParam;
use crate::utils::fs::calculate_sha1;

const UPDATE_STATE_FILE_NAME: &str = ".sjmcl-github-update-state.json";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubModpackFile {
  pub path: String,
  pub sha1: String,
  #[serde(default)]
  pub size: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubModpackManifest {
  pub name: String,
  pub version: String,
  #[serde(default)]
  pub base_url: Option<String>,
  #[serde(default)]
  pub files: Vec<GithubModpackFile>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubModpackUpdateState {
  #[serde(default)]
  pub version: String,
  #[serde(default)]
  pub files: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubModpackUpdateInfo {
  pub name: String,
  pub current_version: Option<String>,
  pub latest_version: String,
  pub files_to_download: Vec<GithubModpackFile>,
  pub files_to_remove: Vec<String>,
  pub total_size: u64,
  pub manifest_url: String,
}

fn sanitize_relative_path(path: &str) -> BGUMCLResult<PathBuf> {
  let normalized = path.replace('\\', "/");
  let p = Path::new(&normalized);
  let valid = !normalized.is_empty()
    && !normalized.starts_with('/')
    && p.components()
      .all(|component| matches!(component, Component::Normal(_)));
  if !valid {
    return Err(BGUMCLError(format!("Unsafe path in modpack manifest: {}", path)));
  }
  Ok(PathBuf::from(&normalized))
}

fn resolve_base_url(manifest_url: &str, manifest: &GithubModpackManifest) -> String {
  if let Some(base_url) = &manifest.base_url {
    return base_url.trim_end_matches('/').to_string();
  }
  let trimmed = manifest_url.trim_end_matches('/');
  match trimmed.rfind('/') {
    Some(idx) => trimmed[..idx].to_string(),
    None => trimmed.to_string(),
  }
}

fn build_file_url(base_url: &str, rel_path: &str) -> BGUMCLResult<url::Url> {
  let encoded = rel_path
    .replace('\\', "/")
    .split('/')
    .map(|segment| urlencoding::encode(segment).into_owned())
    .collect::<Vec<String>>()
    .join("/");
  let full = format!("{}/{}", base_url.trim_end_matches('/'), encoded);
  url::Url::parse(&full).map_err(|e| BGUMCLError(format!("Invalid file url {}: {}", full, e)))
}

/// GitHub blob pages return HTML, but the update system needs the raw JSON.
/// Convert `...github.com/owner/repo/blob/branch/path` to
/// `...raw.githubusercontent.com/owner/repo/branch/path`, keeping any proxy
/// prefix (e.g. `https://v4.gh-proxy.org/https://`) so acceleration still works.
fn normalize_manifest_url(raw: &str) -> String {
  const BLOB_MARKER: &str = "/blob/";
  const HOST_MARKER: &str = "github.com/";
  if let Some(blob_idx) = raw.find(BLOB_MARKER) {
    if let Some(host_idx) = raw[..blob_idx].rfind(HOST_MARKER) {
      let prefix = &raw[..host_idx];
      let owner_repo = &raw[host_idx + HOST_MARKER.len()..blob_idx];
      let branch_path = &raw[blob_idx + BLOB_MARKER.len()..];
      return format!(
        "{}raw.githubusercontent.com/{}/{}",
        prefix, owner_repo, branch_path
      );
    }
  }
  raw.to_string()
}

async fn fetch_manifest(_app: &AppHandle, manifest_url: &str) -> BGUMCLResult<GithubModpackManifest> {
  let normalized = normalize_manifest_url(manifest_url);
  // Try accelerated mirrors first (v4 -> cdn), then a direct connection, so
  // modpack updates still work in regions where one proxy is unreachable.
  let candidates = crate::utils::web::gh_proxy_candidates(&normalized);
  let client = crate::tasks::download::download_client().clone();
  let mut last_err: Option<String> = None;
  let mut resp: Option<reqwest::Response> = None;
  for candidate in &candidates {
    match client.get(candidate).send().await {
      Ok(r) if r.status().is_success() => {
        resp = Some(r);
        break;
      }
      Ok(r) => last_err = Some(format!("HTTP {}", r.status())),
      Err(e) => last_err = Some(format!("{:?}", e)),
    }
  }
  let resp = resp.ok_or_else(|| {
    BGUMCLError(format!(
      "Failed to fetch modpack manifest: {}",
      last_err.unwrap_or_else(|| "unknown error".to_string())
    ))
  })?;
  let manifest: GithubModpackManifest = resp
    .json()
    .await
    .map_err(|e| BGUMCLError(format!("Failed to parse modpack manifest: {:?}", e)))?;
  for file in &manifest.files {
    sanitize_relative_path(&file.path)?;
  }
  Ok(manifest)
}

fn state_path(instance: &Instance) -> PathBuf {
  instance.version_path.join(UPDATE_STATE_FILE_NAME)
}

async fn load_state(instance: &Instance) -> BGUMCLResult<GithubModpackUpdateState> {
  let path = state_path(instance);
  if !path.exists() {
    return Ok(GithubModpackUpdateState::default());
  }
  Ok(load_json_async::<GithubModpackUpdateState>(&path).await?)
}

fn get_instance(app: &AppHandle, instance_id: &str) -> BGUMCLResult<Instance> {
  let binding = app.state::<Mutex<HashMap<String, Instance>>>();
  let state = binding
    .lock()
    .map_err(|_| BGUMCLError("Failed to lock instance state".to_string()))?;
  state
    .get(instance_id)
    .cloned()
    .ok_or_else(|| BGUMCLError(format!("Instance not found: {}", instance_id)))
}

fn get_instance_root(app: &AppHandle, instance_id: &str) -> BGUMCLResult<PathBuf> {
  get_instance_subdir_path_by_id(app, &instance_id.to_string(), &InstanceSubdirType::Root)
    .ok_or_else(|| BGUMCLError(format!("Instance not found: {}", instance_id)))
}

fn compute_update_plan(
  root: &Path,
  state: &GithubModpackUpdateState,
  manifest: &GithubModpackManifest,
) -> BGUMCLResult<(Vec<GithubModpackFile>, Vec<String>, u64)> {
  let mut files_to_download = Vec::new();
  let mut total_size = 0u64;

  for file in &manifest.files {
    let rel = sanitize_relative_path(&file.path)?;
    let local = root.join(&rel);
    let local_sha = calculate_sha1(&local)?;
    let up_to_date =
      matches!(&local_sha, Some(sha) if sha.eq_ignore_ascii_case(&file.sha1));
    if !up_to_date {
      total_size = total_size.saturating_add(file.size);
      files_to_download.push(file.clone());
    }
  }

  let files_to_remove = state
    .files
    .keys()
    .filter(|rel| !manifest.files.iter().any(|file| file.path == **rel))
    .cloned()
    .collect();

  Ok((files_to_download, files_to_remove, total_size))
}

fn verify_downloaded_files(root: &Path, files: &[GithubModpackFile]) -> BGUMCLResult<()> {
  for file in files {
    let rel = sanitize_relative_path(&file.path)?;
    let local = root.join(&rel);
    let local_sha = calculate_sha1(&local)?;
    let ok = matches!(&local_sha, Some(sha) if sha.eq_ignore_ascii_case(&file.sha1));
    if !ok {
      return Err(BGUMCLError(format!(
        "Modpack update incomplete, missing or mismatched file: {}",
        file.path
      )));
    }
  }
  Ok(())
}

#[tauri::command]
pub async fn check_github_modpack_update(
  app: AppHandle,
  instance_id: String,
) -> BGUMCLResult<GithubModpackUpdateInfo> {
  let instance = get_instance(&app, &instance_id)?;
  let manifest_url = instance
    .modpack_update_channel
    .clone()
    .ok_or_else(|| {
      BGUMCLError("No modpack update channel configured for this instance".to_string())
    })?;

  let manifest = fetch_manifest(&app, &manifest_url).await?;
  let root = get_instance_root(&app, &instance_id)?;
  let state = load_state(&instance).await?;
  let (files_to_download, files_to_remove, total_size) =
    compute_update_plan(&root, &state, &manifest)?;

  Ok(GithubModpackUpdateInfo {
    name: manifest.name.clone(),
    current_version: instance.modpack_version.clone(),
    latest_version: manifest.version.clone(),
    files_to_download,
    files_to_remove,
    total_size,
    manifest_url,
  })
}

#[tauri::command]
pub async fn apply_github_modpack_update(
  app: AppHandle,
  instance_id: String,
) -> BGUMCLResult<GithubModpackUpdateInfo> {
  let instance = get_instance(&app, &instance_id)?;
  let manifest_url = instance
    .modpack_update_channel
    .clone()
    .ok_or_else(|| {
      BGUMCLError("No modpack update channel configured for this instance".to_string())
    })?;

  let manifest = fetch_manifest(&app, &manifest_url).await?;
  let root = get_instance_root(&app, &instance_id)?;
  let state = load_state(&instance).await?;
  let (files_to_download, files_to_remove, total_size) =
    compute_update_plan(&root, &state, &manifest)?;

  let base_url = resolve_base_url(&manifest_url, &manifest);
  let mut params = Vec::new();
  for file in &files_to_download {
    params.push(PTaskParam::Download(DownloadParam {
      src: build_file_url(&base_url, &file.path)?,
      dest: root.join(sanitize_relative_path(&file.path)?),
      filename: None,
      sha1: Some(file.sha1.clone()),
    }));
  }

  if !params.is_empty() {
    let task_group = format!("modpack-github-update?{}", instance_id);
    let desc = schedule_progressive_task_group(app.clone(), task_group, params, true).await?;

    let monitor = app.state::<std::pin::Pin<Box<TaskMonitor>>>();
    monitor.wait_for_group_completion(&desc.task_group).await?;
    verify_downloaded_files(&root, &files_to_download)?;
  }

  finalize_update(&app, &instance, &manifest, &files_to_remove).await?;

  Ok(GithubModpackUpdateInfo {
    name: manifest.name.clone(),
    current_version: instance.modpack_version.clone(),
    latest_version: manifest.version.clone(),
    files_to_download,
    files_to_remove,
    total_size,
    manifest_url,
  })
}

#[tauri::command]
pub async fn set_github_modpack_update_channel(
  app: AppHandle,
  instance_id: String,
  manifest_url: String,
) -> BGUMCLResult<()> {
  let instance = get_instance(&app, &instance_id)?;
  let trimmed = manifest_url.trim().to_string();
  if trimmed.is_empty() {
    return Err(BGUMCLError("Manifest url cannot be empty".to_string()));
  }

  let mut updated = instance.clone();
  updated.modpack_update_channel = Some(trimmed.clone());
  updated.save_json_cfg().await?;

  {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let mut state = binding
      .lock()
      .map_err(|_| BGUMCLError("Failed to lock instance state".to_string()))?;
    if let Some(inst) = state.get_mut(&instance_id) {
      inst.modpack_update_channel = Some(trimmed);
    }
  }

  Ok(())
}

#[tauri::command]
pub async fn set_modpack_version(
  app: AppHandle,
  instance_id: String,
  version: String,
) -> BGUMCLResult<()> {
  let instance = get_instance(&app, &instance_id)?;
  let trimmed = version.trim().to_string();
  if trimmed.is_empty() {
    return Err(BGUMCLError("Version cannot be empty".to_string()));
  }
  let mut updated = instance.clone();
  updated.modpack_version = Some(trimmed.clone());
  updated.save_json_cfg().await?;
  {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let mut state = binding
      .lock()
      .map_err(|_| BGUMCLError("Failed to lock instance state".to_string()))?;
    if let Some(inst) = state.get_mut(&instance_id) {
      inst.modpack_version = Some(trimmed);
    }
  }
  Ok(())
}
async fn finalize_update(
  app: &AppHandle,
  instance: &Instance,
  manifest: &GithubModpackManifest,
  files_to_remove: &[String],
) -> BGUMCLResult<()> {
  let root = get_instance_root(app, &instance.id)?;

  for rel in files_to_remove {
    let rel_path = sanitize_relative_path(rel)?;
    let target = root.join(rel_path);
    if target.is_file() {
      std::fs::remove_file(&target)
        .map_err(|e| BGUMCLError(format!("Failed to remove {}: {}", target.display(), e)))?;
    }
  }

  let mut new_state = GithubModpackUpdateState {
    version: manifest.version.clone(),
    files: HashMap::new(),
  };
  for file in &manifest.files {
    new_state
      .files
      .insert(file.path.clone(), file.sha1.clone());
  }
  save_json_async(&new_state, &state_path(instance)).await?;

  let mut updated = instance.clone();
  updated.modpack_version = Some(manifest.version.clone());
  updated.save_json_cfg().await?;

  {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let mut state = binding
      .lock()
      .map_err(|_| BGUMCLError("Failed to lock instance state".to_string()))?;
    if let Some(inst) = state.get_mut(&instance.id) {
      inst.modpack_version = Some(manifest.version.clone());
    }
  }

  Ok(())
}
