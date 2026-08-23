use serde::{Deserialize, Serialize};
use serde_json::Value;
use sjmcl_types::error::BGUMCLResult;
use tauri::{AppHandle, Manager};
use tauri_plugin_http::reqwest;

use crate::instance::models::misc::ModLoaderType;
use crate::resource::helpers::misc::get_download_api;
use crate::resource::models::{ModLoaderResourceInfo, ResourceError, ResourceType, SourceType};

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct FabricMetaItem {
  pub loader: FabricLoaderInfo,
  pub intermediary: Value,
  pub launcher_meta: Value,
}

#[derive(Serialize, Deserialize, Default)]
struct FabricLoaderInfo {
  pub separator: String,
  pub build: i64,
  pub maven: String,
  pub version: String,
  pub stable: bool,
}

pub async fn get_fabric_meta_by_game_version(
  app: &AppHandle,
  priority_list: &[SourceType],
  game_version: &str,
) -> BGUMCLResult<Vec<ModLoaderResourceInfo>> {
  let client = app.state::<reqwest::Client>();
  let mut saw_parse_error = false;
  for source_type in priority_list.iter() {
    let url = get_download_api(*source_type, ResourceType::FabricMeta)?
      .join("v2/versions/loader/")?
      .join(game_version)?;
    match client
      .get(url.clone())
      .header("accept", "application/json")
      .header("accept-encoding", "identity")
      .send()
      .await
    {
      Ok(response) => {
        if response.status().is_success() {
          match response.bytes().await {
            Ok(body) => match serde_json::from_slice::<Vec<FabricMetaItem>>(&body) {
              Ok(manifest) => {
                return Ok(
                  manifest
                    .into_iter()
                    .map(|info| ModLoaderResourceInfo {
                      loader_type: ModLoaderType::Fabric,
                      version: info.loader.version,
                      description: String::new(),
                      // stable: info.loader.stable,
                      stable: None,
                      branch: None,
                    })
                    .collect(),
                );
              }
              Err(error) => {
                saw_parse_error = true;
                log::warn!(
                  "Fabric metadata JSON parse failed from {} ({} bytes): {}",
                  url,
                  body.len(),
                  error
                );
              }
            },
            Err(error) => log::warn!("Fabric metadata body read failed from {}: {}", url, error),
          }
        } else {
          log::warn!(
            "Fabric metadata source {} returned HTTP {}",
            url,
            response.status()
          );
        }
      }
      Err(error) => log::warn!("Fabric metadata request failed for {}: {}", url, error),
    }
  }
  if saw_parse_error {
    Err(ResourceError::ParseError.into())
  } else {
    Err(ResourceError::NetworkError.into())
  }
}
