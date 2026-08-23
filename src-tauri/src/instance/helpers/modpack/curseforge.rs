use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sjmcl_types::error::{BGUMCLError, BGUMCLResult};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use tauri::{AppHandle, Manager};
use tokio::sync::Semaphore;
use tauri_plugin_http::reqwest;
use zip::ZipArchive;

use crate::instance::helpers::modpack::import::{ModpackManifest, ModpackMetaInfo};
use crate::instance::models::misc::{InstanceError, ModLoader, ModLoaderType};
use crate::resource::helpers::curseforge::misc::{CURSEFORGE_API_KEY, CurseForgeProject};
use crate::resource::models::OtherResourceSource;
use crate::tasks::PTaskParam;
use crate::tasks::download::DownloadParam;

const CURSEFORGE_METADATA_RETRIES: usize = 3;

async fn fetch_curseforge_json<T: DeserializeOwned>(
  client: &reqwest::Client,
  url: &str,
  parse_error: InstanceError,
) -> BGUMCLResult<T> {
  let mut candidates = Vec::new();
  if let Some(mirror) = crate::utils::web::mcim_mirror_url(url) {
    candidates.push(mirror);
  }
  candidates.push(url.to_string());
  candidates.dedup();

  let mut saw_parse_error = false;
  for candidate in candidates {
    for attempt in 0..CURSEFORGE_METADATA_RETRIES {
      let response = match client
        .get(&candidate)
        .header("x-api-key", CURSEFORGE_API_KEY.as_str())
        .header("accept", "application/json")
        .header("accept-encoding", "identity")
        .send()
        .await
      {
        Ok(response) => response,
        Err(error) => {
          log::warn!(
            "CurseForge metadata request failed (attempt {}/{}): {} ({})",
            attempt + 1,
            CURSEFORGE_METADATA_RETRIES,
            candidate,
            error
          );
          if attempt + 1 < CURSEFORGE_METADATA_RETRIES {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
          }
          continue;
        }
      };

      if !response.status().is_success() {
        log::warn!(
          "CurseForge metadata source returned {}: {}",
          response.status(),
          candidate
        );
        break;
      }

      let body = match response.bytes().await {
        Ok(body) => body,
        Err(error) => {
          saw_parse_error = true;
          log::warn!("CurseForge metadata body decode failed for {}: {}", candidate, error);
          continue;
        }
      };

      match serde_json::from_slice::<T>(&body) {
        Ok(value) => return Ok(value),
        Err(error) => {
          saw_parse_error = true;
          log::warn!(
            "CurseForge metadata JSON parse failed for {} ({} bytes): {}",
            candidate,
            body.len(),
            error
          );
        }
      }

      if attempt + 1 < CURSEFORGE_METADATA_RETRIES {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
      }
    }
  }

  if saw_parse_error {
    Err(parse_error.into())
  } else {
    Err(InstanceError::NetworkError.into())
  }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CurseForgeModLoader {
  pub id: String,
  pub primary: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CurseForgeFiles {
  #[serde(rename = "projectID")]
  pub project_id: u64,
  #[serde(rename = "fileID")]
  pub file_id: u64,
  pub required: bool,
}

structstruck::strike! {
#[strikethrough[derive(Deserialize, Serialize, Debug, Clone)]]
#[strikethrough[serde(rename_all = "camelCase")]]
  pub struct CurseForgeManifest {
    pub name: String,
    pub version: Option<String>,
    pub author: String,
    pub overrides: String,
    pub minecraft: struct {
      pub version: String,
      pub mod_loaders: Vec<CurseForgeModLoader>,
    },
    pub files: Vec<CurseForgeFiles>,
  }
}

structstruck::strike! {
#[strikethrough[derive(Deserialize, Serialize, Debug, Clone)]]
#[strikethrough[serde(rename_all = "camelCase")]]
  pub struct CurseForgeFileManifest {
    pub data: struct {
      pub download_url: Option<String>,
      pub file_name: String,
      pub hashes: Option<Vec<pub struct {
        pub value: String,
        pub algo: u64,
      }>>,
    }
  }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CurseForgeProjectRes {
  pub data: CurseForgeProject,
}

#[async_trait]
impl ModpackManifest for CurseForgeManifest {
  fn from_archive(file: &File) -> BGUMCLResult<Self> {
    let mut archive = ZipArchive::new(file)?;
    let mut manifest_file = archive.by_name("manifest.json")?;
    let mut manifest_content = String::new();
    manifest_file.read_to_string(&mut manifest_content)?;
    let manifest: Self = serde_json::from_str(&manifest_content).inspect_err(|e| {
      eprintln!("{:?}", e);
    })?;

    Ok(manifest)
  }

  async fn get_meta_info(&self, app: &AppHandle) -> BGUMCLResult<ModpackMetaInfo> {
    let client_version = self.get_client_version()?;
    let mod_loader = if let Ok((loader_type, version)) = self.get_mod_loader_type_version() {
      let loader = ModLoader {
        loader_type,
        version,
        ..Default::default()
      };
      if matches!(loader.loader_type, ModLoaderType::Forge) {
        Some(loader.with_branch(app, client_version.clone()).await?)
      } else {
        Some(loader)
      }
    } else {
      None
    };
    Ok(ModpackMetaInfo {
      name: self.name.clone(),
      version: self.version.clone(),
      description: None,
      author: Some(self.author.clone()),
      modpack_source: OtherResourceSource::CurseForge,
      client_version,
      mod_loader,
    })
  }

  fn get_client_version(&self) -> BGUMCLResult<String> {
    Ok(self.minecraft.version.clone())
  }

  fn get_mod_loader_type_version(&self) -> BGUMCLResult<(ModLoaderType, String)> {
    let loader = self
      .minecraft
      .mod_loaders
      .iter()
      .find(|l| l.primary)
      .ok_or(InstanceError::ModLoaderVersionParseError)?;

    let Some((loader_type, version)) = loader.id.split_once('-') else {
      return Err(InstanceError::ModLoaderVersionParseError.into());
    };
    Ok((
      ModLoaderType::from_str(loader_type)
        .ok()
        .ok_or(InstanceError::ModLoaderVersionParseError)?,
      version.to_string(),
    ))
  }

  async fn get_download_params(
    &self,
    app: &AppHandle,
    instance_path: &Path,
  ) -> BGUMCLResult<Vec<PTaskParam>> {
    let client = app.state::<reqwest::Client>();
    let instance_path = instance_path.to_path_buf();

    // The manifest commonly contains many files from the same project. PCL
    // avoids serial metadata work here; cache project categories once so an
    // import does not issue one identical project request per file.
    let mut project_ids: Vec<u64> = self.files.iter().map(|file| file.project_id).collect();
    project_ids.sort_unstable();
    project_ids.dedup();
    // CurseForge imports can contain hundreds of files. Keep API metadata
    // requests bounded so one slow connection does not starve the launcher or
    // trigger rate limiting.
    let metadata_gate = Arc::new(Semaphore::new(8));
    let project_tasks = project_ids.iter().map(|project_id| {
      let client = client.clone();
      let project_id = *project_id;
      let metadata_gate = metadata_gate.clone();
      async move {
        let _permit = metadata_gate
          .acquire_owned()
          .await
          .map_err(|_| BGUMCLError("CurseForge metadata gate closed".to_string()))?;
        let project: CurseForgeProjectRes = fetch_curseforge_json(
          &client,
          &format!("https://api.curseforge.com/v1/mods/{project_id}"),
          InstanceError::CurseForgeFileManifestParseError,
        )
        .await?;
        Ok::<(u64, Option<i32>), BGUMCLError>((project_id, project.data.class_id))
      }
    });
    let class_ids: HashMap<u64, Option<i32>> = futures::future::join_all(project_tasks)
      .await
      .into_iter()
      .collect::<BGUMCLResult<HashMap<_, _>>>()?;

    let tasks = self.files.iter().map(|file| {
      let client = client.clone();
      let instance_path = instance_path.clone();
      let metadata_gate = metadata_gate.clone();
      let file_id = file.file_id;
      let project_id = file.project_id;
      let class_id = class_ids.get(&project_id).copied().flatten();

      async move {
        let _permit = metadata_gate
          .acquire_owned()
          .await
          .map_err(|_| BGUMCLError("CurseForge metadata gate closed".to_string()))?;
        let file_manifest: CurseForgeFileManifest = {
          fetch_curseforge_json(
            &client,
            &format!(
              "https://api.curseforge.com/v1/mods/{project_id}/files/{file_id}"
            ),
            InstanceError::CurseForgeFileManifestParseError,
          )
          .await?
        };

        let download_url = file_manifest.data.download_url.unwrap_or(format!(
          "https://edge.forgecdn.net/files/{}/{}/{}",
          file_id / 1000,
          file_id % 1000,
          urlencoding::encode(&file_manifest.data.file_name)
        ));

        let sha1 = file_manifest
          .data
          .hashes
          .as_ref()
          .and_then(|hs| hs.iter().find(|h| h.algo == 1))
          .map(|h| h.value.clone());

        let task_param = PTaskParam::Download(DownloadParam {
          src: url::Url::parse(&download_url).map_err(|_| InstanceError::InvalidSourcePath)?,
          sha1,
          dest: instance_path
            .join(match class_id {
              Some(12) | Some(6945) => "resourcepacks",
              Some(6552) => "shaderpacks",
              _ => "mods",
            })
            .join(&file_manifest.data.file_name),
          filename: Some(file_manifest.data.file_name.clone()),
        });

        Ok::<PTaskParam, BGUMCLError>(task_param)
      }
    });

    let results = futures::future::join_all(tasks).await;

    let mut task_params = Vec::new();
    for result in results {
      task_params.push(result?);
    }
    Ok(task_params)
  }

  fn get_overrides_path(&self) -> String {
    self.overrides.clone()
  }
}
