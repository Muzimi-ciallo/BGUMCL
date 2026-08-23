use regex::Regex;
use sjmcl_types::error::BGUMCLResult;
use std::ffi::OsStr;
use tauri::Manager;
use tauri_plugin_http::reqwest;

use crate::resource::helpers::misc::get_download_api;
use crate::resource::models::{OptiFineResourceInfo, ResourceError, ResourceType, SourceType};
use crate::utils::fs::split_filename;

fn get_optifine_sort_key(info: &OptiFineResourceInfo) -> (u32, u32, u32) {
  let Some((_, suffix)) = info.filename.rsplit_once("_HD_U_") else {
    return (0, 0, 0);
  };
  let (version, pre) = match suffix.split_once("_pre") {
    Some((version, pre)) => (version, pre.parse().unwrap_or(0)),
    None => (suffix, u32::MAX),
  };
  let mut chars = version.chars();
  let prefix = chars
    .next()
    .map(|ch| ch.to_ascii_uppercase() as u32)
    .unwrap_or(0);
  let series = chars.as_str().parse().unwrap_or(0);
  (prefix, series, pre)
}

async fn get_optifine_meta_by_game_version_bmcl(
  app: &tauri::AppHandle,
  game_version: &str,
) -> BGUMCLResult<Vec<OptiFineResourceInfo>> {
  let client = app.state::<reqwest::Client>();
  let url =
    get_download_api(SourceType::BMCLAPIMirror, ResourceType::OptiFine)?.join(game_version)?;
  match client.get(url).send().await {
    Ok(response) => {
      if response.status().is_success() {
        let mut manifest = response
          .json::<Vec<OptiFineResourceInfo>>()
          .await
          .map_err(|_| ResourceError::ParseError)?;

        manifest.iter_mut().for_each(|info| {
          info.filename = split_filename(OsStr::new(&info.filename)).0;
        });
        manifest.sort_by(|a, b| {
          get_optifine_sort_key(b)
            .cmp(&get_optifine_sort_key(a))
            .then_with(|| b.filename.cmp(&a.filename))
        });

        Ok(manifest)
      } else {
        Err(ResourceError::NetworkError.into())
      }
    }
    Err(_) => Err(ResourceError::NetworkError.into()),
  }
}

async fn get_optifine_meta_by_game_version_official(
  app: &tauri::AppHandle,
  game_version: &str,
) -> BGUMCLResult<Vec<OptiFineResourceInfo>> {
  let client = app.state::<reqwest::Client>();
  let response = client
    .get("https://optifine.net/downloads")
    .header("Accept", "text/html")
    .send()
    .await
    .map_err(|_| ResourceError::NetworkError)?;
  if !response.status().is_success() {
    return Err(ResourceError::NetworkError.into());
  }
  let html = response
    .text()
    .await
    .map_err(|_| ResourceError::ParseError)?;
  if html.len() < 200 {
    return Err(ResourceError::ParseError.into());
  }

  let filename_re = Regex::new(r##"OptiFine_([0-9A-Za-z_.]+)\.jar["']"##).unwrap();
  let forge_re = Regex::new(r##"(?s)colForge['"][^>]*>\s*([^<]*)"##).unwrap();
  let date_re = Regex::new(r##"(?s)colDate['"][^>]*>\s*([^<]*)"##).unwrap();
  let names = filename_re
    .captures_iter(&html)
    .filter_map(|capture| capture.get(1).map(|value| value.as_str().to_string()))
    .filter(|name| name.starts_with(&format!("{game_version}_")))
    .collect::<Vec<_>>();
  let forge = forge_re
    .captures_iter(&html)
    .filter_map(|capture| {
      capture
        .get(1)
        .map(|value| value.as_str().trim().to_string())
    })
    .collect::<Vec<_>>();
  let dates = date_re
    .captures_iter(&html)
    .filter_map(|capture| {
      capture
        .get(1)
        .map(|value| value.as_str().trim().to_string())
    })
    .collect::<Vec<_>>();

  if names.is_empty() {
    return Err(ResourceError::ParseError.into());
  }

  let mut result = Vec::new();
  for (index, name) in names.iter().enumerate() {
    let suffix = name
      .strip_prefix(&format!("{game_version}_"))
      .unwrap_or_default();
    let Some(patch) = suffix.strip_prefix("HD_U_") else {
      continue;
    };
    let _ = dates.get(index);
    let _ = forge.get(index);
    result.push(OptiFineResourceInfo {
      filename: format!("OptiFine_{name}"),
      patch: patch.to_string(),
      r#type: "HD_U".to_string(),
    });
  }
  if result.is_empty() {
    return Err(ResourceError::ParseError.into());
  }
  result.sort_by(|a, b| {
    get_optifine_sort_key(b)
      .cmp(&get_optifine_sort_key(a))
      .then_with(|| b.filename.cmp(&a.filename))
  });
  Ok(result)
}

pub async fn get_optifine_meta_by_game_version(
  app: &tauri::AppHandle,
  priority_list: &[SourceType],
  game_version: &str,
) -> BGUMCLResult<Vec<OptiFineResourceInfo>> {
  for source_type in priority_list.iter() {
    match source_type {
      SourceType::BMCLAPIMirror => {
        if let Ok(result) = get_optifine_meta_by_game_version_bmcl(app, game_version).await {
          return Ok(result);
        }
      }
      SourceType::Official => {
        if let Ok(result) = get_optifine_meta_by_game_version_official(app, game_version).await {
          return Ok(result);
        }
      }
    }
  }
  Err(ResourceError::NetworkError.into())
}
