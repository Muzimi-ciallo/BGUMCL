use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sjmcl_types::error::BGUMCLResult;
use std::fs;
use std::fs::File;
use std::path::{Component, Path};
use tauri::AppHandle;
use zip::ZipArchive;

use crate::instance::helpers::modpack::curseforge::CurseForgeManifest;
use crate::instance::helpers::modpack::modrinth::ModrinthManifest;
use crate::instance::helpers::modpack::multimc::MultiMcManifest;
use crate::instance::models::misc::{InstanceError, ModLoader, ModLoaderType};
use crate::resource::commands::fetch_mod_loader_version_list;
use crate::resource::models::OtherResourceSource;
use crate::tasks::PTaskParam;

#[async_trait]
pub trait ModpackManifest {
  fn from_archive(file: &File) -> BGUMCLResult<Self>
  where
    Self: Sized;
  fn get_client_version(&self) -> BGUMCLResult<String>;
  fn get_mod_loader_type_version(&self) -> BGUMCLResult<(ModLoaderType, String)>;
  async fn get_meta_info(&self, app: &AppHandle) -> BGUMCLResult<ModpackMetaInfo>;
  async fn get_download_params(
    &self,
    app: &AppHandle,
    instance_path: &Path,
  ) -> BGUMCLResult<Vec<PTaskParam>>;
  fn get_overrides_path(&self) -> String;
  fn get_override_paths(&self) -> Vec<String> {
    vec![self.get_overrides_path()]
  }
}

pub(crate) fn is_safe_relative_modpack_path(path: &str) -> bool {
  if path.is_empty()
    || path.starts_with('/')
    || path.starts_with('\\')
    || path.as_bytes().get(1) == Some(&b':')
  {
    return false;
  }

  !Path::new(path).components().any(|component| {
    matches!(
      component,
      Component::Prefix(_) | Component::RootDir | Component::ParentDir
    )
  })
}

type ManifestBox = Box<dyn ModpackManifest + Send + Sync>;
type Parser = Box<dyn Fn(&File) -> BGUMCLResult<ManifestBox> + Send + Sync>;

fn get_parsers() -> Vec<Parser> {
  vec![
    Box::new(|f| {
      CurseForgeManifest::from_archive(f).map(|m| {
        let b: ManifestBox = Box::new(m);
        b
      })
    }),
    Box::new(|f| {
      ModrinthManifest::from_archive(f).map(|m| {
        let b: ManifestBox = Box::new(m);
        b
      })
    }),
    Box::new(|f| {
      MultiMcManifest::from_archive(f).map(|m| {
        let b: ManifestBox = Box::new(m);
        b
      })
    }),
  ]
}

impl ModLoader {
  pub async fn with_branch(&self, app: &AppHandle, mc_version: String) -> BGUMCLResult<Self> {
    let version_list =
      fetch_mod_loader_version_list(app.clone(), mc_version, self.loader_type).await?;
    if let Some(version) = version_list.iter().find(|v| v.version == self.version) {
      return Ok(Self {
        branch: version.branch.clone(),
        ..self.clone()
      });
    }
    Err(InstanceError::ModLoaderVersionParseError.into())
  }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ModpackMetaInfo {
  pub name: String,
  pub version: Option<String>,
  pub description: Option<String>,
  pub author: Option<String>,
  pub modpack_source: OtherResourceSource,
  pub client_version: String,
  pub mod_loader: Option<ModLoader>,
}

impl ModpackMetaInfo {
  pub async fn from_archive(app: &AppHandle, file: &File) -> BGUMCLResult<Self> {
    for parser in get_parsers() {
      if let Ok(manifest) = parser(file) {
        return manifest.get_meta_info(app).await;
      }
    }

    Err(InstanceError::ModpackManifestParseError.into())
  }
}

pub async fn get_download_params(
  app: &AppHandle,
  file: &File,
  instance_path: &Path,
) -> BGUMCLResult<Vec<PTaskParam>> {
  for parser in get_parsers() {
    if let Ok(manifest) = parser(file) {
      return manifest.get_download_params(app, instance_path).await;
    }
  }

  Err(InstanceError::ModpackManifestParseError.into())
}

pub fn extract_overrides(file: &File, instance_path: &Path) -> BGUMCLResult<()> {
  let get_override_paths = |file| {
    for parser in get_parsers() {
      if let Ok(manifest) = parser(file) {
        return Some(manifest.get_override_paths());
      }
    }
    None
  };
  let override_paths = get_override_paths(file).ok_or(InstanceError::ModpackManifestParseError)?;
  let mut archive = ZipArchive::new(file)?;
  for override_path in override_paths {
    let prefix = override_path.trim_matches(['/', '\\']);
    if prefix.is_empty() {
      continue;
    }

    for i in 0..archive.len() {
      let mut file = archive.by_index(i)?;
      let archive_path = file.name().replace('\\', "/");
      let prefix_with_separator = format!("{prefix}/");
      let Some(relative_path) = archive_path.strip_prefix(&prefix_with_separator) else {
        continue;
      };
      if !is_safe_relative_modpack_path(relative_path) || !file.is_file() {
        continue;
      }

      let outpath = instance_path.join(relative_path);
      if let Some(parent) = outpath.parent()
        && !parent.exists()
      {
        fs::create_dir_all(parent)?;
      }

      let mut outfile = File::create(&outpath)?;
      std::io::copy(&mut file, &mut outfile)?;
    }
  }
  Ok(())
}
