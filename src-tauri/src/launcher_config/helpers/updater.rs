use serde_json::Value;
use sjmcl_types::error::{BGUMCLError, BGUMCLResult};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager};
use tauri_plugin_http::reqwest;

use crate::launcher_config::models::{LauncherConfig, LauncherConfigError};
use crate::tasks::PTaskParam;
use crate::tasks::commands::schedule_progressive_task_group;
use crate::tasks::download::DownloadParam;

const MANIFEST_URL: &str = "https://cdn.jsdelivr.net/gh/Muzimi-ciallo/BGUMCL@main/update.json";
const DOWNLOAD_BASE_URL: &str = "https://github.com/Muzimi-ciallo/BGUMCL/releases/download";

// Generate the new version filename on remote origin according to the current os, arch and is_portable
fn build_resource_filename(ver: &str, os: &str, arch: &str) -> String {
  let arch = if arch == "x86" { "i686" } else { arch };
  let suffix = match os {
    "windows" => "_portable.exe",
    "linux" => "_portable",
    "macos" => ".app.tar.gz",
    _ => "",
  };
  format!("BGUMCL_{}_{}_{}{}", ver, os, arch, suffix)
}

// Generate the new filename on the local disk.
// If old_name contains old_version, replace the first occurrence with new_version.
// Otherwise, keep the old_name unchanged.
fn build_local_new_filename(old_name: &str, old_version: &str, new_version: &str) -> String {
  if let Some(idx) = old_name.find(old_version) {
    let mut s = String::with_capacity(old_name.len() - old_version.len() + new_version.len());
    s.push_str(&old_name[..idx]);
    s.push_str(new_version);
    s.push_str(&old_name[idx + old_version.len()..]);
    s
  } else {
    old_name.to_string()
  }
}

pub async fn fetch_latest_version(
  app: &AppHandle,
) -> BGUMCLResult<Option<(String, String, String, String, String)>> {
  let config_binding = app.state::<Mutex<LauncherConfig>>();
  let (os, arch, _is_china_mainland_ip) = {
    let config_state = config_binding.lock()?;
    (
      config_state.basic_info.os_type.clone(),
      config_state.basic_info.arch.clone(),
      config_state.basic_info.is_china_mainland_ip,
    )
  };
  let client = app.state::<reqwest::Client>();

  let resp = client
    .get(MANIFEST_URL)
    .send()
    .await
    .map_err(|_| LauncherConfigError::FetchError)?;
  let j: Value = resp
    .json()
    .await
    .map_err(|_| LauncherConfigError::FetchError)?;

  let Some(ver) = j.get("version").and_then(|v| v.as_str()) else {
    return Err(LauncherConfigError::FetchError.into());
  };
  let ver = ver.to_string();
  let base_url = j
    .get("base_url")
    .and_then(|v| v.as_str())
    .unwrap_or(DOWNLOAD_BASE_URL)
    .trim_end_matches('/')
    .to_string();
  let fname = build_resource_filename(&ver, os.as_str(), arch.as_str());
  let download_url = format!("{}/{}", base_url, fname);

  let release_notes = j
    .get("release_notes")
    .and_then(|v| v.as_str())
    .unwrap_or_default()
    .to_string();
  let published_at = j
    .get("published_at")
    .and_then(|v| v.as_str())
    .unwrap_or_default()
    .to_string();

  Ok(Some((ver, fname, download_url, release_notes, published_at)))
}

pub async fn download_target_version(
  app: &AppHandle,
  version: String,
  fname: String,
  download_url: Option<String>,
) -> BGUMCLResult<()> {
  let config_binding = app.state::<Mutex<LauncherConfig>>();
  let download_cache_dir = {
    let config_state = config_binding.lock()?;
    config_state.download.cache.directory.clone()
  };

  let url = match download_url {
    Some(u) => u,
    None => format!("{}/v{}/{}", DOWNLOAD_BASE_URL, version, fname),
  };

  schedule_progressive_task_group(
    app.clone(),
    format!("launcher-update?{}", fname),
    vec![PTaskParam::Download(DownloadParam {
      src: url::Url::parse(&url).map_err(|_| LauncherConfigError::FetchError)?,
      dest: download_cache_dir.join(&fname),
      filename: Some(fname),
      sha1: None,
    })],
    true,
  )
  .await?;

  Ok(())
}
#[cfg(target_os = "windows")]
pub async fn install_update_windows(
  app: &AppHandle,
  downloaded_filename: String,
  restart: bool,
) -> BGUMCLResult<()> {
  use std::os::windows::process::CommandExt;

  let config_binding = app.state::<Mutex<LauncherConfig>>();
  let (old_version, downloaded_path, new_version) = {
    let config_state = config_binding.lock()?;
    (
      config_state.basic_info.launcher_version.clone(),
      config_state
        .download
        .cache
        .directory
        .join(&downloaded_filename),
      downloaded_filename
        .split('_')
        .nth(1)
        .map(|s| s.to_string())
        .unwrap_or_else(|| config_state.basic_info.launcher_version.clone()),
    )
  };
  let cur_exe = std::env::current_exe()?;

  // Portable: replace current exe with the newly downloaded one via a temp cmd script.
    let cur_dir = cur_exe
      .parent()
      .ok_or_else(|| BGUMCLError("No parent dir for exe".to_string()))?;
    let old_name = cur_exe
      .file_name()
      .and_then(|s| s.to_str())
      .ok_or_else(|| BGUMCLError("Invalid exe name".to_string()))?
      .to_string();

    let target_name = build_local_new_filename(&old_name, &old_version, &new_version);
    let target = cur_dir.join(target_name);
    let pid = std::process::id().to_string();
    let restart_flag = if restart { "1" } else { "0" };

    // write and execute a PowerShell script to wait -> replace -> start -> cleanup
    let script_path = app
      .path()
      .resolve::<PathBuf>("update.ps1".into(), BaseDirectory::AppCache)?;
    let script_content = r#"param(
  [string]$ProcessId,
  [string]$Downloaded,
  [string]$Target,
  [string]$OldExe,
  [string]$Restart
)

try {
  while (Get-Process -Id $ProcessId -ErrorAction SilentlyContinue) {
    Start-Sleep -Milliseconds 200
  }

  if (Test-Path -LiteralPath $Target) { Remove-Item -LiteralPath $Target -Force -ErrorAction SilentlyContinue }
  if (Test-Path -LiteralPath $OldExe) { Remove-Item -LiteralPath $OldExe -Force -ErrorAction SilentlyContinue }

  Move-Item -LiteralPath $Downloaded -Destination $Target -Force

  if ($Restart -eq '1') {
    Start-Process -FilePath $Target
  }
} catch {
  Write-Error $_.Exception.Message
  exit 1
}
"#;

    fs::write(&script_path, script_content.as_bytes())?;
    let _ = Command::new("powershell.exe")
      .arg("-NoProfile")
      .arg("-ExecutionPolicy")
      .arg("Bypass")
      .arg("-File")
      .arg(&script_path)
      .arg(&pid)
      .arg(&downloaded_path)
      .arg(&target)
      .arg(&cur_exe)
      .arg(restart_flag)
      .creation_flags(0x08000000)
      .spawn()?;

    if restart {
      app.exit(0);
    }
    Ok(())

}

#[cfg(target_os = "macos")]
pub async fn install_update_macos(
  app: &AppHandle,
  downloaded_filename: String,
  restart: bool,
) -> BGUMCLResult<()> {
  use std::ffi::OsStr;

  let config_binding = app.state::<Mutex<LauncherConfig>>();
  let (old_version, downloaded_path, new_version) = {
    let config_state = config_binding.lock()?;
    (
      config_state.basic_info.launcher_version.clone(),
      config_state
        .download
        .cache
        .directory
        .join(&downloaded_filename),
      downloaded_filename
        .clone()
        .split('_')
        .nth(1)
        .map(|s| s.to_string())
        .unwrap_or_else(|| config_state.basic_info.launcher_version.clone()),
    )
  };
  let cur_exe = std::env::current_exe()?;

  // find app bundle folder by walking up from executable
  let app_bundle = cur_exe
    .ancestors()
    .find(|p| p.extension().and_then(OsStr::to_str) == Some("app"))
    .ok_or_else(|| BGUMCLError("Not inside .app bundle".to_string()))?
    .to_path_buf();
  let app_dir = app_bundle
    .parent()
    .ok_or_else(|| BGUMCLError("No parent dir for .app".to_string()))?
    .to_path_buf();
  let old_name = app_bundle
    .file_name()
    .and_then(|s| s.to_str())
    .ok_or_else(|| BGUMCLError("Invalid .app name".to_string()))?
    .to_string();

  let target_name = build_local_new_filename(&old_name, &old_version, &new_version);
  let target_app = app_dir.join(target_name);
  let pid = std::process::id().to_string();
  let restart_flag = if restart { "1" } else { "0" };

  // write and execute a bash script to wait -> replace -> start -> cleanup
  let script_path = app
    .path()
    .resolve::<PathBuf>("update.sh".to_string().into(), BaseDirectory::AppCache)?;

  let script_content = r#"#!/bin/bash
set -e
PID="$1"
DOWNLOADED="$2"
TARGET_APP="$3"
OLD_APP="$4"
RESTART="$5"

# wait until current process exits
while kill -0 $PID 2>/dev/null; do sleep 0.2; done

TMPDIR="$(mktemp -d)"
tar -xzf "$DOWNLOADED" -C "$TMPDIR"
NEW_APP="$(find "$TMPDIR" -maxdepth 1 -name "*.app" | head -n 1)"
if [ -z "$NEW_APP" ]; then
  echo "No .app found in archive" >&2
  exit 1
fi

rm -rf "$TARGET_APP" || true
rm -rf "$OLD_APP" || true
mv "$NEW_APP" "$TARGET_APP"

if [ "$RESTART" = "1" ]; then
  open -a "$TARGET_APP"
fi

rm -rf "$TMPDIR" || true
"#;

  fs::write(&script_path, script_content.as_bytes())?;
  let _ = Command::new("chmod").arg("+x").arg(&script_path).status();
  let _ = Command::new("bash")
    .arg(&script_path)
    .arg(&pid)
    .arg(&downloaded_path)
    .arg(&target_app)
    .arg(&app_bundle)
    .arg(restart_flag)
    .spawn()?;

  if restart {
    app.exit(0);
  }
  Ok(())
}


