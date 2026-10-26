use futures::{StreamExt, stream};
use serde::Serialize;
use sha1::Digest;
use sjmcl_types::error::{BGUMCLError, BGUMCLResult};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;
use zip::ZipArchive;

use crate::instance::helpers::misc::get_instance_game_config;
use crate::instance::helpers::modpack::curseforge::CurseForgeManifest;
use crate::instance::helpers::modpack::import::{ModpackManifest, is_safe_relative_modpack_path};
use crate::instance::helpers::modpack::modrinth::ModrinthManifest;
use crate::instance::helpers::modpack::multimc::MultiMcManifest;
use crate::instance::models::misc::{Instance, InstanceError};
use crate::launcher_config::models::LauncherConfig;
use crate::tasks::PTaskParam;
use crate::tasks::commands::schedule_progressive_task_group;
use crate::tasks::download::DownloadParam;
use crate::tasks::events::GEventStatus;
use crate::tasks::monitor::TaskMonitor;

#[derive(Clone, Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum IntegrityStatus {
  Valid,
  Missing,
  Corrupted,
  Disabled,
  Optional,
  Unverified,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrityEntry {
  pub id: String,
  pub path: String,
  pub status: IntegrityStatus,
  pub source: String,
  pub detail: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrityReport {
  pub scan_id: String,
  pub pack_name: String,
  pub entries: Vec<IntegrityEntry>,
}

#[derive(Clone)]
enum RepairSource {
  Remote(DownloadParam),
  Archive(usize),
}

#[derive(Clone)]
struct ExpectedMod {
  path: String,
  sha1: String,
  sha512: Option<String>,
  optional: bool,
  source: RepairSource,
}

#[derive(Clone)]
struct ScanPlan {
  instance_id: String,
  root: PathBuf,
  archive_path: PathBuf,
  archive_sha256: String,
  mods: HashMap<String, ExpectedMod>,
}

#[derive(Default)]
pub struct IntegrityScans(Mutex<HashMap<String, ScanPlan>>);

#[tauri::command]
pub fn open_download_cache(app: AppHandle) -> BGUMCLResult<()> {
  let directory = app
    .state::<Mutex<LauncherConfig>>()
    .lock()
    .map_err(|_| BGUMCLError("Launcher configuration unavailable".into()))?
    .download
    .cache
    .directory
    .clone();
  std::fs::create_dir_all(&directory)?;
  app
    .opener()
    .open_path(directory.to_string_lossy(), None::<&str>)
    .map_err(|error| BGUMCLError(error.to_string()))
}

fn instance_root(app: &AppHandle, id: &str) -> BGUMCLResult<(PathBuf, String)> {
  let instance = app
    .state::<Mutex<HashMap<String, Instance>>>()
    .lock()
    .map_err(|_| BGUMCLError("Instance state unavailable".into()))?
    .get(id)
    .cloned()
    .ok_or(InstanceError::InstanceNotFoundByID)?;
  let root = if get_instance_game_config(app, &instance).version_isolation {
    instance.version_path.clone()
  } else {
    instance
      .version_path
      .parent()
      .and_then(Path::parent)
      .ok_or(InstanceError::InvalidSourcePath)?
      .to_path_buf()
  };
  Ok((root.canonicalize()?, instance.version))
}

// Only mod files are in scope. Check the normalized path on every platform,
// including Windows drive/ADS syntax and symlinks beneath the instance root.
fn mod_path(path: &str) -> BGUMCLResult<String> {
  let normalized = path.replace('\\', "/");
  if !is_safe_relative_modpack_path(&normalized)
    || !normalized.starts_with("mods/")
    || normalized.contains(':')
    || normalized
      .split('/')
      .any(|part| part.is_empty() || part == "." || part.ends_with([' ', '.']))
    || !(normalized.ends_with(".jar") || normalized.ends_with(".jar.disabled"))
  {
    return Err(InstanceError::InvalidSourcePath.into());
  }
  Ok(normalized)
}

fn is_mod_file(path: &str) -> bool {
  path.ends_with(".jar") || path.ends_with(".jar.disabled")
}

fn checked_target(root: &Path, relative: &str) -> BGUMCLResult<PathBuf> {
  let relative = mod_path(relative)?;
  let mut target = root.to_path_buf();
  for part in relative.split('/') {
    target.push(part);
    if let Ok(metadata) = std::fs::symlink_metadata(&target)
      && metadata.file_type().is_symlink()
    {
      return Err(BGUMCLError(format!("Refusing linked mod path: {relative}")));
    }
  }
  Ok(target)
}

fn hash_reader<D: Digest + Default>(mut reader: impl Read) -> BGUMCLResult<String> {
  let mut hash = D::default();
  let mut buffer = [0u8; 65536];
  loop {
    let size = reader.read(&mut buffer)?;
    if size == 0 {
      break;
    }
    hash.update(&buffer[..size]);
  }
  Ok(hex::encode(hash.finalize()))
}

fn classify(root: &Path, expected: &ExpectedMod) -> BGUMCLResult<IntegrityStatus> {
  let target = checked_target(root, &expected.path)?;
  if !expected.path.ends_with(".disabled") {
    let disabled = format!("{}.disabled", expected.path);
    if checked_target(root, &disabled)?.is_file() && !target.exists() {
      return Ok(IntegrityStatus::Disabled);
    }
  }
  if target.is_file() && expected.path.ends_with(".disabled") {
    return Ok(IntegrityStatus::Disabled);
  }
  if expected.sha1.len() != 40 || !expected.sha1.bytes().all(|b| b.is_ascii_hexdigit()) {
    return Err(BGUMCLError("Manifest has no valid SHA-1 checksum".into()));
  }
  if !target.exists() {
    return Ok(if expected.optional {
      IntegrityStatus::Optional
    } else {
      IntegrityStatus::Missing
    });
  }
  if !target.is_file() {
    return Err(BGUMCLError(format!(
      "Mod path is not a file: {}",
      expected.path
    )));
  }
  let valid = hash_reader::<sha1::Sha1>(File::open(&target)?)?.eq_ignore_ascii_case(&expected.sha1);
  let valid = if valid {
    if let Some(sha512) = &expected.sha512 {
      hash_reader::<sha2::Sha512>(File::open(&target)?)?.eq_ignore_ascii_case(sha512)
    } else {
      true
    }
  } else {
    false
  };
  Ok(if valid {
    IntegrityStatus::Valid
  } else {
    IntegrityStatus::Corrupted
  })
}

struct ArchiveInfo {
  name: String,
  version: String,
  sha256: String,
  curseforge: Option<CurseForgeManifest>,
  mods: BTreeMap<String, ExpectedMod>,
}

fn read_archive(path: &Path, root: &Path) -> BGUMCLResult<ArchiveInfo> {
  let file = File::open(path)?;
  let sha256 = hash_reader::<sha2::Sha256>(&file)?;
  // ZipArchive seeks its reader, so hashing must precede parsing.
  let mut archive = ZipArchive::new(&file)?;
  let mut mods = BTreeMap::new();
  let (name, version, prefixes, curseforge) = if archive.by_name("manifest.json").is_ok() {
    let manifest = CurseForgeManifest::from_archive(&file)?;
    (
      manifest.name.clone(),
      manifest.get_client_version()?,
      manifest.get_override_paths(),
      Some(manifest),
    )
  } else if archive.by_name("modrinth.index.json").is_ok() {
    let manifest = ModrinthManifest::from_archive(&file)?;
    for entry in &manifest.files {
      if !entry.path.replace('\\', "/").starts_with("mods/")
        || !is_mod_file(&entry.path)
        || entry
          .env
          .as_ref()
          .is_some_and(|env| env.client == "unsupported")
      {
        continue;
      }
      let relative = mod_path(&entry.path)?;
      let src = entry
        .downloads
        .iter()
        .filter_map(|url| url::Url::parse(url).ok())
        .find(|url| url.scheme() == "https")
        .ok_or(InstanceError::InvalidSourcePath)?;
      mods.insert(
        relative.clone(),
        ExpectedMod {
          path: relative.clone(),
          sha1: entry.hashes.sha1.clone(),
          sha512: Some(entry.hashes.sha512.clone()),
          optional: entry
            .env
            .as_ref()
            .is_some_and(|env| env.client == "optional"),
          source: RepairSource::Remote(DownloadParam {
            src,
            sha1: Some(entry.hashes.sha1.clone()),
            dest: root.join(&relative),
            filename: None,
          }),
        },
      );
    }
    (
      manifest.name.clone(),
      manifest.get_client_version()?,
      manifest.get_override_paths(),
      None,
    )
  } else {
    let manifest = MultiMcManifest::from_archive(&file)?;
    (
      manifest.cfg.get("name").cloned().unwrap_or_default(),
      manifest.get_client_version()?,
      manifest.get_override_paths(),
      None,
    )
  };
  // Later override directories take precedence, matching the importer.
  for prefix in prefixes {
    let prefix = format!("{}/", prefix.replace('\\', "/").trim_matches('/'));
    for index in 0..archive.len() {
      let mut entry = archive.by_index(index)?;
      let name = entry.name().replace('\\', "/");
      let Some(relative) = name.strip_prefix(&prefix) else {
        continue;
      };
      if !entry.is_file() || !relative.starts_with("mods/") || !is_mod_file(relative) {
        continue;
      }
      let relative = mod_path(relative)?;
      let sha1 = hash_reader::<sha1::Sha1>(&mut entry)?;
      mods.insert(
        relative.clone(),
        ExpectedMod {
          path: relative,
          sha1,
          sha512: None,
          optional: false,
          source: RepairSource::Archive(index),
        },
      );
    }
  }
  Ok(ArchiveInfo {
    name,
    version,
    sha256,
    curseforge,
    mods,
  })
}

#[tauri::command]
pub async fn scan_modpack_integrity(
  app: AppHandle,
  instance_id: String,
  archive_path: PathBuf,
) -> BGUMCLResult<IntegrityReport> {
  let (root, version) = instance_root(&app, &instance_id)?;
  let archive_path = archive_path.canonicalize()?;
  let path = archive_path.clone();
  let scan_root = root.clone();
  let mut archive = tokio::task::spawn_blocking(move || read_archive(&path, &scan_root))
    .await
    .map_err(|err| BGUMCLError(err.to_string()))??;
  if archive.version != version {
    return Err(BGUMCLError(format!(
      "Minecraft version mismatch: ZIP {}, instance {}",
      archive.version, version
    )));
  }
  let mut entries = Vec::new();
  if let Some(manifest) = archive.curseforge {
    let results = stream::iter(manifest.files.iter().cloned().map(|entry| {
      let mut single = manifest.clone();
      single.files = vec![entry.clone()];
      let app = app.clone();
      let root = root.clone();
      async move {
        let result = tokio::time::timeout(
          Duration::from_secs(45),
          single.get_download_params(&app, &root),
        )
        .await;
        let result = match result {
          Ok(result) => result,
          Err(_) => Err(BGUMCLError("Mod metadata request timed out".into())),
        };
        (entry, result)
      }
    }))
    .buffer_unordered(8)
    .collect::<Vec<_>>()
    .await;
    for (entry, result) in results {
      match result {
        Ok(params) => {
          for param in params {
            let PTaskParam::Download(download) = param;
            let relative = download
              .dest
              .strip_prefix(&root)
              .map_err(|_| InstanceError::InvalidSourcePath)?
              .to_string_lossy()
              .replace('\\', "/");
            if !relative.starts_with("mods/") || !is_mod_file(&relative) {
              continue;
            }
            let relative = mod_path(&relative)?;
            // Bundled overrides win over remotely declared files.
            archive.mods.entry(relative.clone()).or_insert(ExpectedMod {
              path: relative,
              sha1: download.sha1.clone().unwrap_or_default(),
              sha512: None,
              optional: !entry.required,
              source: RepairSource::Remote(download),
            });
          }
        }
        Err(error) => entries.push(IntegrityEntry {
          id: format!("cf:{}:{}", entry.project_id, entry.file_id),
          path: format!("CurseForge {}/{}", entry.project_id, entry.file_id),
          status: IntegrityStatus::Unverified,
          source: "remote".into(),
          detail: Some(error.0),
        }),
      }
    }
  }
  let scan_root = root.clone();
  let mods = archive.mods;
  let (classified, mods) = tokio::task::spawn_blocking(move || {
    let classified = mods
      .values()
      .map(|expected| {
        let (status, detail) = match classify(&scan_root, expected) {
          Ok(status) => (status, None),
          Err(error) => (IntegrityStatus::Unverified, Some(error.0)),
        };
        IntegrityEntry {
          id: expected.path.clone(),
          path: expected.path.clone(),
          status,
          source: match expected.source {
            RepairSource::Remote(_) => "remote",
            RepairSource::Archive(_) => "archive",
          }
          .into(),
          detail,
        }
      })
      .collect::<Vec<_>>();
    (classified, mods.into_iter().collect())
  })
  .await
  .map_err(|error| BGUMCLError(error.to_string()))?;
  entries.extend(classified);
  entries.sort_by(|a, b| a.path.cmp(&b.path));
  let scan_id = uuid::Uuid::new_v4().to_string();
  let state = app.state::<IntegrityScans>();
  let mut plans = state
    .0
    .lock()
    .map_err(|_| BGUMCLError("Scan state unavailable".into()))?;
  plans.retain(|_, plan| plan.instance_id != instance_id);
  if plans.len() >= 8 {
    plans.clear();
  }
  plans.insert(
    scan_id.clone(),
    ScanPlan {
      instance_id,
      root,
      archive_path,
      archive_sha256: archive.sha256,
      mods,
    },
  );
  Ok(IntegrityReport {
    scan_id,
    pack_name: archive.name,
    entries,
  })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrityRepairResult {
  pub restored: usize,
  pub queued: usize,
}

#[tauri::command]
pub async fn repair_modpack_integrity(
  app: AppHandle,
  instance_id: String,
  scan_id: String,
  selected_ids: Vec<String>,
) -> BGUMCLResult<IntegrityRepairResult> {
  let monitor = app.state::<Pin<Box<TaskMonitor>>>();
  if monitor.has_active_download_tasks()
    || monitor
      .state_list()
      .iter()
      .any(|group| matches!(group.status, GEventStatus::Started | GEventStatus::Stopped))
  {
    return Err(BGUMCLError(
      "Wait for current downloads to finish before repairing mods".into(),
    ));
  }
  let plan = app
    .state::<IntegrityScans>()
    .0
    .lock()
    .map_err(|_| BGUMCLError("Scan state unavailable".into()))?
    .remove(&scan_id)
    .ok_or_else(|| BGUMCLError("Scan expired; scan the original ZIP again".into()))?;
  if plan.instance_id != instance_id || instance_root(&app, &instance_id)?.0 != plan.root {
    return Err(InstanceError::InvalidSourcePath.into());
  }
  let (params, restored) = tokio::task::spawn_blocking(move || prepare_repair(plan, selected_ids))
    .await
    .map_err(|error| BGUMCLError(error.to_string()))??;
  let queued = params.len();
  if queued > 0 {
    schedule_progressive_task_group(
      app.clone(),
      format!("modpack-integrity?{instance_id}"),
      params,
      true,
    )
    .await?;
  }
  Ok(IntegrityRepairResult { restored, queued })
}

fn prepare_repair(
  plan: ScanPlan,
  selected_ids: Vec<String>,
) -> BGUMCLResult<(Vec<PTaskParam>, usize)> {
  let file = File::open(&plan.archive_path)?;
  if hash_reader::<sha2::Sha256>(&file)? != plan.archive_sha256 {
    return Err(BGUMCLError("Original ZIP changed; scan it again".into()));
  }
  let mut archive = ZipArchive::new(&file)?;
  let mut params = Vec::new();
  let mut restored = 0;
  let mut selected = HashSet::new();
  for id in selected_ids {
    if !selected.insert(id.clone()) {
      continue;
    }
    let expected = plan.mods.get(&id).ok_or(InstanceError::InvalidSourcePath)?;
    // Recheck disk state: a mod may have been disabled or completed since scanning.
    if !matches!(
      classify(&plan.root, expected)?,
      IntegrityStatus::Missing | IntegrityStatus::Corrupted
    ) {
      continue;
    }
    let target = checked_target(&plan.root, &expected.path)?;
    match &expected.source {
      RepairSource::Remote(download) => params.push(PTaskParam::Download(download.clone())),
      RepairSource::Archive(index) => {
        std::fs::create_dir_all(target.parent().ok_or(InstanceError::InvalidSourcePath)?)?;
        let temporary = target.with_extension(format!("bgumcl-repair-{}", uuid::Uuid::new_v4()));
        let result = (|| -> BGUMCLResult<()> {
          let mut source = archive.by_index(*index)?;
          let mut output = File::create(&temporary)?;
          std::io::copy(&mut source, &mut output)?;
          output.flush()?;
          drop(output);
          if hash_reader::<sha1::Sha1>(File::open(&temporary)?)? != expected.sha1 {
            return Err(BGUMCLError("Bundled mod checksum mismatch".into()));
          }
          std::fs::rename(&temporary, &target)?;
          Ok(())
        })();
        let _ = std::fs::remove_file(&temporary);
        result?;
        restored += 1;
      }
    }
  }
  Ok::<_, BGUMCLError>((params, restored))
}

#[cfg(test)]
mod tests {
  use crate::instance::helpers::modpack::integrity::{
    ExpectedMod, IntegrityStatus, RepairSource, ScanPlan, classify, mod_path, prepare_repair,
    read_archive,
  };
  use crate::tasks::PTaskParam;
  use crate::tasks::download::DownloadParam;
  use sha1::Digest;
  use std::fs::{self, File};
  use std::io::Write;
  use std::path::PathBuf;
  use zip::write::SimpleFileOptions;

  struct Fixture(PathBuf);
  impl Fixture {
    fn new() -> Self {
      let root =
        std::env::temp_dir().join(format!("bgumcl-integrity-test-{}", uuid::Uuid::new_v4()));
      fs::create_dir_all(root.join("mods")).unwrap();
      Self(root)
    }

    fn archive(&self, extra: &[(&str, &[u8])]) -> PathBuf {
      let path = self.0.join("original.zip");
      let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
      zip
        .start_file("manifest.json", SimpleFileOptions::default())
        .unwrap();
      zip.write_all(br#"{"name":"Test pack","author":"Test","overrides":"overrides","minecraft":{"version":"1.20.1","modLoaders":[]},"files":[]}"#).unwrap();
      for (name, bytes) in extra {
        zip.start_file(*name, SimpleFileOptions::default()).unwrap();
        zip.write_all(bytes).unwrap();
      }
      zip.finish().unwrap();
      path
    }

    fn plan(&self, extra: &[(&str, &[u8])]) -> ScanPlan {
      let path = self.archive(extra);
      let info = read_archive(&path, &self.0).unwrap();
      ScanPlan {
        instance_id: "test".into(),
        root: self.0.clone(),
        archive_path: path,
        archive_sha256: info.sha256,
        mods: info.mods.into_iter().collect(),
      }
    }
  }
  impl Drop for Fixture {
    fn drop(&mut self) {
      let _ = fs::remove_dir_all(&self.0);
    }
  }

  fn remote() -> ExpectedMod {
    ExpectedMod {
      path: "mods/example.jar".into(),
      sha1: hex::encode(sha1::Sha1::digest(b"correct")),
      sha512: None,
      optional: false,
      source: RepairSource::Remote(DownloadParam {
        src: "https://example.com/example.jar".parse().unwrap(),
        dest: PathBuf::new(),
        filename: None,
        sha1: None,
      }),
    }
  }

  #[test]
  fn detects_missing_corrupt_valid_optional_and_disabled_mods() {
    let fixture = Fixture::new();
    let mut expected = remote();
    assert_eq!(
      classify(&fixture.0, &expected).unwrap(),
      IntegrityStatus::Missing
    );
    expected.optional = true;
    assert_eq!(
      classify(&fixture.0, &expected).unwrap(),
      IntegrityStatus::Optional
    );
    fs::write(fixture.0.join(&expected.path), b"partial").unwrap();
    assert_eq!(
      classify(&fixture.0, &expected).unwrap(),
      IntegrityStatus::Corrupted
    );
    fs::write(fixture.0.join(&expected.path), b"correct").unwrap();
    assert_eq!(
      classify(&fixture.0, &expected).unwrap(),
      IntegrityStatus::Valid
    );
    fs::rename(
      fixture.0.join(&expected.path),
      fixture.0.join("mods/example.jar.disabled"),
    )
    .unwrap();
    assert_eq!(
      classify(&fixture.0, &expected).unwrap(),
      IntegrityStatus::Disabled
    );
  }

  #[test]
  fn missing_checksum_is_unverified_even_when_file_is_absent() {
    let fixture = Fixture::new();
    let mut expected = remote();
    expected.sha1.clear();
    assert!(classify(&fixture.0, &expected).is_err());
  }

  #[test]
  fn repair_only_restores_selected_mods_and_preserves_other_files() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&[
      ("overrides/mods/a.jar", b"good"),
      ("overrides/mods/b.jar", b"other"),
      ("overrides/config/test.toml", b"pack-config"),
      ("overrides/saves/world.dat", b"pack-save"),
    ]);
    fs::create_dir(fixture.0.join("config")).unwrap();
    fs::write(fixture.0.join("config/test.toml"), b"user-config").unwrap();
    fs::write(fixture.0.join("mods/extra.jar"), b"custom").unwrap();
    fs::write(fixture.0.join("mods/a.jar"), b"corrupt").unwrap();
    assert_eq!(plan.mods.len(), 2);
    let (params, restored) =
      prepare_repair(plan, vec!["mods/a.jar".into(), "mods/a.jar".into()]).unwrap();
    assert!(params.is_empty());
    assert_eq!(restored, 1);
    assert_eq!(fs::read(fixture.0.join("mods/a.jar")).unwrap(), b"good");
    assert!(!fixture.0.join("mods/b.jar").exists());
    assert!(!fixture.0.join("saves").exists());
    assert_eq!(
      fs::read(fixture.0.join("config/test.toml")).unwrap(),
      b"user-config"
    );
    assert_eq!(
      fs::read(fixture.0.join("mods/extra.jar")).unwrap(),
      b"custom"
    );
    assert_eq!(fs::read_dir(fixture.0.join("mods")).unwrap().count(), 2);
  }

  #[test]
  fn repair_rechecks_disabled_mods_and_archive_identity() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&[("overrides/mods/a.jar", b"good")]);
    fs::write(fixture.0.join("mods/a.jar.disabled"), b"user-disabled").unwrap();
    let (_, restored) = prepare_repair(plan.clone(), vec!["mods/a.jar".into()]).unwrap();
    assert_eq!(restored, 0);
    assert!(!fixture.0.join("mods/a.jar").exists());
    fs::write(&plan.archive_path, b"replaced zip").unwrap();
    assert!(prepare_repair(plan, vec!["mods/a.jar".into()]).is_err());
  }

  #[test]
  fn remote_repair_returns_existing_download_parameters() {
    let fixture = Fixture::new();
    let mut plan = fixture.plan(&[]);
    let mut expected = remote();
    if let RepairSource::Remote(param) = &mut expected.source {
      param.dest = fixture.0.join(&expected.path);
      param.sha1 = Some(expected.sha1.clone());
    }
    plan.mods.insert(expected.path.clone(), expected.clone());
    let (params, restored) = prepare_repair(plan, vec![expected.path]).unwrap();
    assert_eq!(restored, 0);
    assert_eq!(params.len(), 1);
    let PTaskParam::Download(param) = &params[0];
    assert_eq!(param.dest, fixture.0.join("mods/example.jar"));
    assert_eq!(param.sha1, Some(expected.sha1));
    assert!(!fixture.0.join("mods/example.jar").exists());
  }

  #[test]
  fn modrinth_skips_server_files_and_client_overrides_take_precedence() {
    let fixture = Fixture::new();
    let path = fixture.0.join("pack.mrpack");
    let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
    zip
      .start_file("modrinth.index.json", SimpleFileOptions::default())
      .unwrap();
    let file = |path: &str, client: &str| serde_json::json!({"path":path,"hashes":{"sha1":"a".repeat(40),"sha512":"b".repeat(128)},"env":{"client":client,"server":"required"},"downloads":["https://example.com/mod.jar"],"fileSize":10});
    let manifest = serde_json::json!({"formatVersion":1,"game":"minecraft","versionId":"1","name":"MR Test","dependencies":{"minecraft":"1.20.1"},"files":[file("mods/server.jar","unsupported"),file("mods/client.jar","required"),file("mods/optional.jar","optional")]});
    zip
      .write_all(serde_json::to_string(&manifest).unwrap().as_bytes())
      .unwrap();
    zip
      .start_file("overrides/mods/client.jar", SimpleFileOptions::default())
      .unwrap();
    zip.write_all(b"base").unwrap();
    zip
      .start_file(
        "client-overrides/mods/client.jar",
        SimpleFileOptions::default(),
      )
      .unwrap();
    zip.write_all(b"client").unwrap();
    zip.finish().unwrap();
    let info = read_archive(&path, &fixture.0).unwrap();
    assert_eq!(info.mods.len(), 2);
    assert_eq!(
      info.mods["mods/client.jar"].sha1,
      hex::encode(sha1::Sha1::digest(b"client"))
    );
    assert!(info.mods["mods/optional.jar"].optional);
    assert!(matches!(
      info.mods["mods/client.jar"].source,
      RepairSource::Archive(_)
    ));
  }

  #[test]
  fn rejects_paths_outside_mods_and_windows_aliases() {
    for path in [
      "../mods/a.jar",
      "mods/../a.jar",
      "mods\\..\\a.jar",
      "C:/mods/a.jar",
      "mods/a.jar:stream",
      "config/a.jar",
      "mods/./a.jar",
      "mods//a.jar",
      "mods/sub. /a.jar",
    ] {
      assert!(mod_path(path).is_err(), "accepted {path}");
    }
    assert_eq!(mod_path("mods\\sub\\a.jar").unwrap(), "mods/sub/a.jar");
  }
}
