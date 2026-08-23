use futures::{StreamExt, TryStreamExt};
use lazy_static::lazy_static;
use regex::{Regex, RegexBuilder};
use sanitize_filename;
use sjmcl_types::error::BGUMCLResult;
use sjmcl_types::partial::{PartialError, PartialUpdate};
use sjmcl_types::storage::{Storage, load_json_async, save_json_async};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager};
use tauri_plugin_http::reqwest;
use tokio;
use tokio::sync::Semaphore;
use url::Url;
use zip::read::ZipArchive;

use crate::instance::helpers::client_json::{
  McClientInfo, remove_mod_loader_from_client_info, remove_optifine_from_client_info,
  replace_native_libraries,
};
use crate::instance::helpers::game_version::{build_game_version_cmp_fn, compare_game_versions};
use crate::instance::helpers::loader::common::{execute_processors, install_mod_loader};
use crate::instance::helpers::loader::fabric::remove_fabric_api_mods;
use crate::instance::helpers::loader::forge::InstallProfile;
use crate::instance::helpers::loader::optifine::{
  download_optifine_installer, finish_optifine_install,
};
use crate::instance::helpers::misc::{
  get_instance_game_config, get_instance_subdir_path_by_id, get_instance_subdir_paths,
  refresh_and_update_instances, unify_instance_name,
};
use crate::instance::helpers::modpack::export::{
  ExportModpackOptions, build_export_bundle, create_modpack_zip, list_files,
  validate_export_options,
};
use crate::instance::helpers::modpack::import::{
  ModpackMetaInfo, extract_overrides, get_download_params,
};
use crate::instance::helpers::mods::common::{
  check_potential_incompatibility, compress_icon, get_mod_info_from_dir, get_mod_info_from_jar,
};
use crate::instance::helpers::options_txt::get_minecraft_lang_tag;
use crate::instance::helpers::resourcepack::{
  load_resourcepack_from_dir, load_resourcepack_from_zip,
};
use crate::instance::helpers::server::{
  GameServerInfo, get_servers_nbt_path_by_instance_id, load_servers_info_from_nbt,
  query_servers_online, save_servers_to_nbt,
};
use crate::instance::helpers::world::{load_level_data_from_nbt, load_world_info_from_dir};
use crate::instance::models::misc::{
  Instance, InstanceError, InstanceSubdirType, InstanceSummary, LocalModInfo, ModLoader,
  ModLoaderStatus, ModLoaderType, ModpackFileList, OptiFine, ResourcePackInfo, SchematicInfo,
  ScreenshotInfo, ShaderPackInfo,
};
use crate::instance::models::world::base::WorldInfo;
use crate::instance::models::world::level::LevelData;
use crate::launch::helpers::file_validator::{get_invalid_assets, get_invalid_library_files};
use crate::launch::helpers::jre_selector::{get_minimum_java_version_by_game, select_java_runtime};
use crate::launch::models::LaunchError;
use crate::launcher_config::helpers::java::build_mojang_java_download_params;
use crate::launcher_config::helpers::misc::get_global_game_config;
use crate::launcher_config::models::{GameConfig, GameDirectory, LauncherConfig};
use crate::resource::helpers::misc::get_source_priority_list;
use crate::resource::helpers::translation::{
  LOCAL_MOD_TRANSLATION_CACHE_EXPIRY_HOURS, LocalModTranslationEntry, LocalModTranslationsCache,
  add_local_mod_translations,
};
use crate::resource::models::{
  GameClientResourceInfo, ModLoaderResourceInfo, OptiFineResourceInfo,
};
use crate::tasks::PTaskParam;
use crate::tasks::commands::schedule_progressive_task_group;
use crate::tasks::download::DownloadParam;
use crate::utils::fs::{
  RemoveDirGuard, copy_whole_dir, create_url_shortcut, generate_unique_filename,
  get_files_with_regex, get_files_with_regex_recursive, get_subdirectories,
  normalize_relative_path,
};
use crate::utils::image::ImageWrapper;

#[tauri::command]
pub async fn retrieve_instance_list(app: AppHandle) -> BGUMCLResult<Vec<InstanceSummary>> {
  refresh_and_update_instances(&app, false).await; // firstly refresh and update
  let global_version_isolation = get_global_game_config(&app).version_isolation;
  let mut summary_list = Vec::new();

  let instance_binding = app.state::<Mutex<HashMap<String, Instance>>>();
  let instances = instance_binding.lock().unwrap().clone();
  for (id, instance) in instances.iter() {
    // same as get_game_config(), but mannually here
    let is_version_isolated = if instance.use_spec_game_config
      && let Some(spec_game_config) = instance.spec_game_config.as_ref()
    {
      spec_game_config.version_isolation
    } else {
      global_version_isolation
    };

    summary_list
      .push(InstanceSummary::from_instance(&app, id.clone(), instance, is_version_isolated).await);
  }

  let config_binding = app.state::<Mutex<LauncherConfig>>();
  let mut config_state = config_binding.lock()?;
  // sort instances (starred instance will be pinned to top by frontend)
  let version_cmp_fn = build_game_version_cmp_fn(&app);
  match config_state.states.all_instances_page.sort_by.as_str() {
    "versionAsc" => {
      summary_list.sort_by(|a, b| {
        version_cmp_fn(&a.version, &b.version)
          .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
      });
    }
    "versionDesc" => {
      summary_list.sort_by(|a, b| {
        version_cmp_fn(&b.version, &a.version)
          .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
      });
    }
    _ => {
      summary_list.sort_by_key(|a| a.name.to_lowercase());
    }
  }

  // ensure an instance is selected if instance list is not empty
  if !summary_list.is_empty()
    && !summary_list
      .iter()
      .any(|instance| instance.id == config_state.states.shared.selected_instance_id)
  {
    config_state.partial_update(
      &app,
      "states.shared.selected_instance_id",
      &serde_json::to_string(&summary_list[0].id).unwrap_or_default(),
    )?;
    config_state.save()?;
  }

  Ok(summary_list)
}

#[tauri::command]
pub async fn update_instance_config(
  app: AppHandle,
  instance_id: String,
  key_path: String,
  value: String,
) -> BGUMCLResult<()> {
  let instance = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let mut state = binding.lock().unwrap();
    let instance = state
      .get_mut(&instance_id)
      .ok_or(InstanceError::InstanceNotFoundByID)?;
    let key_path = {
      let mut snake = String::new();
      for (i, ch) in key_path.char_indices() {
        if i > 0 && ch.is_uppercase() {
          snake.push('_');
        }
        snake.push(ch.to_ascii_lowercase());
      }
      snake
    };
    // PartialUpdate not support Option<T> yet
    if key_path == "description" {
      instance.description = serde_json::from_str::<String>(&value).unwrap_or(value);
    } else if key_path == "tag" {
      instance.tag = serde_json::from_str::<Option<String>>(&value)
        .ok()
        .flatten()
        .map(|v| v.trim().to_string());
    } else if key_path == "icon_src" {
      instance.icon_src = serde_json::from_str::<String>(&value).unwrap_or(value);
    } else if key_path == "starred" {
      instance.starred = value.parse::<bool>()?;
    } else if key_path == "use_spec_game_config" {
      let value = value.parse::<bool>()?;
      instance.use_spec_game_config = value;
      if value && instance.spec_game_config.is_none() {
        instance.spec_game_config = Some(get_global_game_config(&app));
      }
    } else if key_path.starts_with("spec_game_config.") {
      let key = key_path.split_at("spec_game_config.".len()).1;
      if let Some(game_config) = instance.spec_game_config.as_mut() {
        game_config.update(key, &value)?;
      }
    } else {
      return Err(PartialError::NotFound.into());
    }
    instance.clone()
  };
  instance.save_json_cfg().await?;
  Ok(())
}

#[tauri::command]
pub fn retrieve_instance_game_config(
  app: AppHandle,
  instance_id: String,
) -> BGUMCLResult<GameConfig> {
  let binding = app.state::<Mutex<HashMap<String, Instance>>>();
  let state = binding.lock().unwrap();
  let instance = state
    .get(&instance_id)
    .ok_or(InstanceError::InstanceNotFoundByID)?;

  Ok(get_instance_game_config(&app, instance))
}

#[tauri::command]
pub async fn reset_instance_game_config(app: AppHandle, instance_id: String) -> BGUMCLResult<()> {
  let instance = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let mut state = binding.lock().unwrap();
    let instance = state
      .get_mut(&instance_id)
      .ok_or(InstanceError::InstanceNotFoundByID)?;
    instance.spec_game_config = Some(get_global_game_config(&app));
    instance.clone()
  };
  instance.save_json_cfg().await?;
  Ok(())
}

#[tauri::command]
pub fn retrieve_instance_subdir_path(
  app: AppHandle,
  instance_id: String,
  dir_type: InstanceSubdirType,
) -> BGUMCLResult<PathBuf> {
  match get_instance_subdir_path_by_id(&app, &instance_id, &dir_type) {
    Some(path) => Ok(path),
    None => Err(InstanceError::InstanceNotFoundByID.into()),
  }
}

// Capability for extensions, CLI and external agents
#[tauri::command]
pub fn read_instance_file(
  app: AppHandle,
  instance_id: String,
  dir_type: InstanceSubdirType,
  path: String,
  mode: Option<String>,
) -> BGUMCLResult<String> {
  let subdir = retrieve_instance_subdir_path(app, instance_id, dir_type)?;
  let relative_path =
    normalize_relative_path(Path::new(&path)).map_err(|_| InstanceError::InvalidSourcePath)?;
  let cano_subdir = fs::canonicalize(subdir)?;
  let cano_target = fs::canonicalize(cano_subdir.join(relative_path))?;

  if !cano_target.starts_with(&cano_subdir) {
    return Err(InstanceError::InvalidSourcePath.into());
  }

  crate::utils::commands::read_file(cano_target.to_string_lossy().into_owned(), mode)
}

#[tauri::command]
pub fn delete_instance(app: AppHandle, instance_id: String) -> BGUMCLResult<()> {
  let instance_binding = app.state::<Mutex<HashMap<String, Instance>>>();
  let instance_state = instance_binding.lock().unwrap();

  let config_binding = app.state::<Mutex<LauncherConfig>>();
  let mut config_state = config_binding.lock()?;

  let instance = instance_state
    .get(&instance_id)
    .ok_or(InstanceError::InstanceNotFoundByID)?;

  let version_path = &instance.version_path;
  let path = Path::new(version_path);

  if path.exists() {
    fs::remove_dir_all(path)?;
  }
  // not update state here. if send success to frontend, it will call retrieve_instance_list and update state there.

  if config_state.states.shared.selected_instance_id == instance_id {
    config_state.partial_update(
      &app,
      "states.shared.selected_instance_id",
      &serde_json::to_string(
        &instance_state
          .keys()
          .next()
          .cloned()
          .unwrap_or_else(|| "".to_string()),
      )
      .unwrap_or_default(),
    )?;
    config_state.save()?;
  }
  Ok(())
}

#[tauri::command]
pub async fn rename_instance(
  app: AppHandle,
  instance_id: String,
  new_name: String,
) -> BGUMCLResult<PathBuf> {
  let new_path = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let mut state = binding.lock().unwrap();
    let instance = match state.get_mut(&instance_id) {
      Some(x) => x,
      None => return Err(InstanceError::InstanceNotFoundByID.into()),
    };
    let new_path = unify_instance_name(&instance.version_path, &new_name)?;

    instance.version_path = new_path.clone();
    instance.name = new_name;
    new_path
  };
  refresh_and_update_instances(&app, false).await;
  Ok(new_path)
}

#[tauri::command]
pub async fn copy_resources_to_instances(
  app: AppHandle,
  src_file_paths: Vec<String>,
  tgt_inst_ids: Vec<String>,
  tgt_dir_type: InstanceSubdirType,
  decompress: bool,
) -> BGUMCLResult<()> {
  if src_file_paths.is_empty() {
    return Err(InstanceError::InvalidSourcePath.into());
  }

  let src_paths = src_file_paths
    .into_iter()
    .map(PathBuf::from)
    .map(|path| {
      if path.is_file() || path.is_dir() {
        Ok(path)
      } else {
        Err(InstanceError::InvalidSourcePath.into())
      }
    })
    .collect::<BGUMCLResult<Vec<_>>>()?;

  let tgt_paths = tgt_inst_ids
    .into_iter()
    .map(|tgt_inst_id| {
      get_instance_subdir_path_by_id(&app, &tgt_inst_id, &tgt_dir_type)
        .ok_or(InstanceError::InstanceNotFoundByID.into())
    })
    .collect::<BGUMCLResult<Vec<_>>>()?;

  for tgt_path in &tgt_paths {
    if !tgt_path.exists() {
      fs::create_dir_all(tgt_path).map_err(|_| InstanceError::FolderCreationFailed)?;
    }
  }

  let entries = src_paths
    .iter()
    .flat_map(|src_path| {
      tgt_paths
        .iter()
        .cloned()
        .map(move |tgt_path| (src_path.clone(), tgt_path))
    })
    .collect::<Vec<_>>();

  let semaphore = Arc::new(Semaphore::new(
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
  ));

  let copy_resource_entry_to_instance =
    |src_path: &Path, tgt_path: &Path, decompress: bool| -> BGUMCLResult<()> {
      if src_path.is_file() {
        let file_name = src_path
          .file_name()
          .ok_or(InstanceError::InvalidSourcePath)?;

        if decompress {
          let base_name = src_path
            .extension()
            .and_then(|ext| if ext == "zip" { Some(()) } else { None })
            .and_then(|_| Path::new(file_name).file_stem())
            .unwrap_or(file_name);
          let dest_path = generate_unique_filename(tgt_path, base_name);

          let file = fs::File::open(src_path).map_err(|_| InstanceError::ZipFileProcessFailed)?;
          let mut archive =
            ZipArchive::new(file).map_err(|_| InstanceError::ZipFileProcessFailed)?;

          fs::create_dir_all(&dest_path).map_err(|_| InstanceError::FolderCreationFailed)?;

          archive
            .extract(&dest_path)
            .map_err(|_| InstanceError::ZipFileProcessFailed)?;
        } else {
          let dest_path = generate_unique_filename(tgt_path, file_name);
          fs::copy(src_path, &dest_path).map_err(|_| InstanceError::FileCopyFailed)?;
        }
      } else if src_path.is_dir() {
        let dir_name = src_path
          .file_name()
          .ok_or(InstanceError::InvalidSourcePath)?;
        let dest_path = generate_unique_filename(tgt_path, dir_name);
        copy_whole_dir(src_path, &dest_path).map_err(|_| InstanceError::FileCopyFailed)?;
      } else {
        return Err(InstanceError::InvalidSourcePath.into());
      }

      Ok(())
    };

  futures::stream::iter(entries)
    .map(Ok::<_, sjmcl_types::error::BGUMCLError>)
    .try_for_each_concurrent(None, move |(src_path, tgt_path)| {
      let semaphore = semaphore.clone();

      async move {
        let permit = semaphore
          .acquire_owned()
          .await
          .map_err(|_| InstanceError::SemaphoreAcquireFailed)?;

        tokio::task::spawn_blocking(move || -> BGUMCLResult<()> {
          let _permit = permit;
          copy_resource_entry_to_instance(&src_path, &tgt_path, decompress)
        })
        .await
        .map_err(|_| InstanceError::FileCopyFailed)??;

        Ok::<(), sjmcl_types::error::BGUMCLError>(())
      }
    })
    .await?;

  Ok(())
}

#[tauri::command]
pub fn move_resource_to_instance(
  app: AppHandle,
  src_file_path: String,
  tgt_inst_id: String,
  tgt_dir_type: InstanceSubdirType,
) -> BGUMCLResult<()> {
  let tgt_path = match get_instance_subdir_path_by_id(&app, &tgt_inst_id, &tgt_dir_type) {
    Some(path) => path,
    None => return Err(InstanceError::InstanceNotFoundByID.into()),
  };

  let src_path = Path::new(&src_file_path);
  if !src_path.is_dir() && !src_path.is_file() {
    return Err(InstanceError::InvalidSourcePath.into());
  }

  let file_name = src_path
    .file_name()
    .ok_or(InstanceError::InvalidSourcePath)?;

  if !tgt_path.exists() {
    fs::create_dir_all(&tgt_path).map_err(|_| InstanceError::FolderCreationFailed)?;
  }

  let dest_path = generate_unique_filename(&tgt_path, file_name);
  fs::rename(&src_file_path, &dest_path).map_err(|_| InstanceError::FileMoveFailed)?;
  Ok(())
}

#[tauri::command]
pub async fn retrieve_world_list(
  app: AppHandle,
  instance_id: String,
) -> BGUMCLResult<Vec<WorldInfo>> {
  let game_version = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let state = binding.lock()?;
    let instance = state
      .get(&instance_id)
      .ok_or(InstanceError::InstanceNotFoundByID)?;
    instance.version.clone()
  };

  // difficulty setting was introduced in game version 14w02a
  let has_difficulty_support = compare_game_versions(&app, &game_version, "14w02a", false)
    .await
    .is_ge();

  let mut world_list: Vec<WorldInfo> = Vec::new();

  let worlds_dir =
    match get_instance_subdir_path_by_id(&app, &instance_id, &InstanceSubdirType::Saves) {
      Some(path) => path,
      None => return Ok(Vec::new()),
    };
  if let Ok(world_paths) = get_subdirectories(worlds_dir) {
    for path in world_paths {
      if let Ok(info) = load_world_info_from_dir(&path, has_difficulty_support).await {
        world_list.push(info);
      }
    }
  }

  Ok(world_list)
}

#[tauri::command]
pub async fn retrieve_game_server_list(
  app: AppHandle,
  instance_id: String,
  query_online: bool,
) -> BGUMCLResult<Vec<GameServerInfo>> {
  // query_online is false, return local data from nbt (servers.dat)
  let nbt_path = match get_servers_nbt_path_by_instance_id(&app, &instance_id) {
    Some(path) => path,
    None => return Ok(Vec::new()),
  };
  let mut game_servers = match load_servers_info_from_nbt(&nbt_path).await {
    Ok(servers) => servers,
    Err(_) => return Err(InstanceError::ServerNbtReadError.into()),
  };

  // skip hidden servers
  game_servers.retain(|server| !server.hidden);

  // query_online is true, amend query and return player count and online status
  if query_online {
    game_servers = query_servers_online(game_servers).await?;
  }

  Ok(game_servers)
}

#[tauri::command]
pub async fn delete_game_server(
  app: AppHandle,
  instance_id: String,
  server_addr: String,
) -> BGUMCLResult<()> {
  let nbt_path = match get_servers_nbt_path_by_instance_id(&app, &instance_id) {
    Some(path) => path,
    None => return Err(InstanceError::InstanceNotFoundByID.into()),
  };
  let mut existing_servers = load_servers_info_from_nbt(&nbt_path).await?;

  existing_servers.retain(|server| server.ip != server_addr);
  save_servers_to_nbt(&nbt_path, &existing_servers)
    .await
    .map_err(|_| InstanceError::FileOperationError)?;

  Ok(())
}

#[tauri::command]
pub async fn add_game_server(
  app: AppHandle,
  instance_id: String,
  server_addr: String,
  server_name: String,
) -> BGUMCLResult<()> {
  let nbt_path = match get_servers_nbt_path_by_instance_id(&app, &instance_id) {
    Some(path) => path,
    None => return Err(InstanceError::InstanceNotFoundByID.into()),
  };
  let mut existing_servers = load_servers_info_from_nbt(&nbt_path).await?;

  if existing_servers
    .iter()
    .any(|server| server.ip == server_addr)
  {
    return Err(InstanceError::DuplicateServer.into());
  }

  existing_servers.push(GameServerInfo {
    ip: server_addr,
    name: server_name,
    ..Default::default()
  });
  save_servers_to_nbt(&nbt_path, &existing_servers)
    .await
    .map_err(|_| InstanceError::FileOperationError)?;

  Ok(())
}

#[tauri::command]
pub async fn retrieve_local_mod_list(
  app: AppHandle,
  instance_id: String,
) -> BGUMCLResult<Vec<LocalModInfo>> {
  let (installed_loader_type, game_version) = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let state = binding.lock().unwrap();
    let instance = state
      .get(&instance_id)
      .ok_or(InstanceError::InstanceNotFoundByID)?;

    let loader_type = instance.mod_loader.loader_type;
    (
      (loader_type != ModLoaderType::Unknown).then_some(loader_type),
      instance.version.clone(),
    )
  };

  let mods_dir = match get_instance_subdir_path_by_id(&app, &instance_id, &InstanceSubdirType::Mods)
  {
    Some(path) => path,
    None => return Ok(Vec::new()),
  };

  let valid_extensions = RegexBuilder::new(r"\.(jar|zip)(\.disabled)*$")
    .case_insensitive(true)
    .build()
    .unwrap();

  let mod_paths = get_files_with_regex(&mods_dir, &valid_extensions).unwrap_or_default();
  let mut tasks = Vec::new();
  let semaphore = Arc::new(Semaphore::new(
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
  ));
  for path in mod_paths {
    let permit = semaphore
      .clone()
      .acquire_owned()
      .await
      .map_err(|_| InstanceError::SemaphoreAcquireFailed)?;
    let task = tokio::spawn(async move {
      log::debug!("Load mod info from jar: {}", path.display());
      let info = get_mod_info_from_jar(&path, installed_loader_type)
        .await
        .ok();
      drop(permit);
      info
    });
    tasks.push(task);
  }
  #[cfg(debug_assertions)]
  {
    // mod information detection from folders is only used for debugging.
    let mod_paths = get_subdirectories(&mods_dir).unwrap_or_default();
    for path in mod_paths {
      let permit = semaphore
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| InstanceError::SemaphoreAcquireFailed)?;
      let task = tokio::spawn(async move {
        log::debug!("Load mod info from dir: {}", path.display());
        let info = get_mod_info_from_dir(&path, installed_loader_type)
          .await
          .ok();
        drop(permit);
        info
      });
      tasks.push(task);
    }
  }
  let mut mod_infos = Vec::new();
  for task in tasks {
    if let Ok(Some(mod_info)) = task.await {
      mod_infos.push(mod_info);
    }
  }

  // check potential incompatibility
  check_potential_incompatibility(&mut mod_infos, installed_loader_type, &game_version);

  // Add translations for mod names and descriptions concurrently
  let mut translation_tasks = Vec::new();
  for mut mod_info in mod_infos {
    let app = app.clone();
    let permit = semaphore
      .clone()
      .acquire_owned()
      .await
      .map_err(|_| InstanceError::SemaphoreAcquireFailed)?;
    let task = tokio::spawn(async move {
      log::debug!("Translating mod: {}", mod_info.file_name);
      let _ = add_local_mod_translations(&app, &mut mod_info).await;
      drop(permit);
      mod_info
    });
    translation_tasks.push(task);
  }
  let mut mod_infos = Vec::new();
  for task in translation_tasks {
    if let Ok(mod_info) = task.await {
      mod_infos.push(mod_info);
    }
  }
  // sort by name (and version)
  mod_infos.sort();
  let local_mod_translations_cache_state = app.state::<Mutex<LocalModTranslationsCache>>();
  let mut cache = local_mod_translations_cache_state.lock()?;
  for info in mod_infos.iter() {
    if let Some(entry) = cache.translations.get(&info.file_name)
      && !entry.is_expired(LOCAL_MOD_TRANSLATION_CACHE_EXPIRY_HOURS)
    {
      continue;
    }
    cache.translations.insert(
      info.file_name.clone(),
      LocalModTranslationEntry::new(
        info.translated_name.clone(),
        info.translated_description.clone(),
      ),
    );
  }
  cache.save()?;

  Ok(mod_infos)
}

#[tauri::command]
pub async fn retrieve_resource_pack_list(
  app: AppHandle,
  instance_id: String,
) -> BGUMCLResult<Vec<ResourcePackInfo>> {
  // Get the resource packs list based on the instance
  let resource_packs_dir =
    match get_instance_subdir_path_by_id(&app, &instance_id, &InstanceSubdirType::ResourcePacks) {
      Some(path) => path,
      None => return Ok(Vec::new()),
    };
  let mut info_list: Vec<ResourcePackInfo> = Vec::new();

  let valid_extensions = RegexBuilder::new(r"\.zip$")
    .case_insensitive(true)
    .build()
    .unwrap();

  for path in get_files_with_regex(&resource_packs_dir, &valid_extensions).unwrap_or(vec![]) {
    if let Ok((description, icon_src)) = load_resourcepack_from_zip(&path) {
      let name = match path.file_stem() {
        Some(stem) => stem.to_string_lossy().to_string(),
        None => String::new(),
      };
      info_list.push(ResourcePackInfo {
        name,
        description,
        icon_src: icon_src.map(ImageWrapper::from).map(compress_icon),
        file_path: path.clone(),
      });
    }
  }

  for path in get_subdirectories(&resource_packs_dir).unwrap_or(vec![]) {
    if let Ok((description, icon_src)) = load_resourcepack_from_dir(&path).await {
      let name = match path.file_stem() {
        Some(stem) => stem.to_string_lossy().to_string(),
        None => String::new(),
      };
      info_list.push(ResourcePackInfo {
        name,
        description,
        icon_src: icon_src.map(ImageWrapper::from).map(compress_icon),
        file_path: path.clone(),
      });
    }
  }
  Ok(info_list)
}

#[tauri::command]
pub async fn retrieve_server_resource_pack_list(
  app: AppHandle,
  instance_id: String,
) -> BGUMCLResult<Vec<ResourcePackInfo>> {
  let resource_packs_dir = match get_instance_subdir_path_by_id(
    &app,
    &instance_id,
    &InstanceSubdirType::ServerResourcePacks,
  ) {
    Some(path) => path,
    None => return Ok(Vec::new()),
  };
  let mut info_list: Vec<ResourcePackInfo> = Vec::new();

  let valid_extensions = RegexBuilder::new(r".*")
    .case_insensitive(true)
    .build()
    .unwrap();

  for path in get_files_with_regex(&resource_packs_dir, &valid_extensions).unwrap_or(vec![]) {
    if let Ok((description, icon_src)) = load_resourcepack_from_zip(&path) {
      let name = match path.file_stem() {
        Some(stem) => stem.to_string_lossy().to_string(),
        None => String::new(),
      };
      info_list.push(ResourcePackInfo {
        name,
        description,
        icon_src: icon_src.map(ImageWrapper::from).map(compress_icon),
        file_path: path.clone(),
      });
    }
  }

  for path in get_subdirectories(&resource_packs_dir).unwrap_or(vec![]) {
    if let Ok((description, icon_src)) = load_resourcepack_from_dir(&path).await {
      let name = match path.file_stem() {
        Some(stem) => stem.to_string_lossy().to_string(),
        None => String::new(),
      };

      info_list.push(ResourcePackInfo {
        name,
        description,
        icon_src: icon_src.map(ImageWrapper::from).map(compress_icon),
        file_path: path.clone(),
      });
    }
  }
  Ok(info_list)
}

#[tauri::command]
pub fn retrieve_schematic_list(
  app: AppHandle,
  instance_id: String,
) -> BGUMCLResult<Vec<SchematicInfo>> {
  let schematics_dir =
    match get_instance_subdir_path_by_id(&app, &instance_id, &InstanceSubdirType::Schematics) {
      Some(path) => path,
      None => return Ok(Vec::new()),
    };

  if !schematics_dir.exists() {
    return Ok(Vec::new());
  }
  let valid_extensions = RegexBuilder::new(r"\.(litematic|schematic)$")
    .case_insensitive(true)
    .build()
    .unwrap();
  let mut schematic_list = Vec::new();
  for schematic_path in
    get_files_with_regex_recursive(schematics_dir.as_path(), &valid_extensions, Some(3))?
  {
    let Ok(relative_path) = schematic_path
      .strip_prefix(&schematics_dir)
      .map(Path::to_path_buf)
    else {
      continue;
    };
    schematic_list.push(SchematicInfo {
      name: schematic_path
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string(),
      file_path: schematic_path,
      relative_path,
    });
  }
  schematic_list.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));

  Ok(schematic_list)
}

#[tauri::command]
pub fn retrieve_shader_pack_list(
  app: AppHandle,
  instance_id: String,
) -> BGUMCLResult<Vec<ShaderPackInfo>> {
  // Get the shaderpacks directory based on the instance
  let shaderpacks_dir =
    match get_instance_subdir_path_by_id(&app, &instance_id, &InstanceSubdirType::ShaderPacks) {
      Some(path) => path,
      None => return Ok(Vec::new()),
    };

  if !shaderpacks_dir.exists() {
    return Ok(Vec::new());
  }

  let valid_extensions = RegexBuilder::new(r"\.zip$")
    .case_insensitive(true)
    .build()
    .unwrap();
  let mut shaderpack_list = Vec::new();
  for path in get_files_with_regex(shaderpacks_dir, &valid_extensions)? {
    shaderpack_list.push(ShaderPackInfo {
      file_name: path.file_stem().unwrap().to_string_lossy().to_string(),
      file_path: path,
    });
  }

  Ok(shaderpack_list)
}

#[tauri::command]
pub fn retrieve_screenshot_list(
  app: AppHandle,
  instance_id: String,
) -> BGUMCLResult<Vec<ScreenshotInfo>> {
  let screenshots_dir =
    match get_instance_subdir_path_by_id(&app, &instance_id, &InstanceSubdirType::Screenshots) {
      Some(path) => path,
      None => return Ok(Vec::new()),
    };

  if !screenshots_dir.exists() {
    return Ok(Vec::new());
  }

  // The default screenshot format in Minecraft is PNG. For broader compatibility, JPG and JPEG formats are also included here.
  let valid_extensions = RegexBuilder::new(r"\.(jpg|jpeg|png)$")
    .case_insensitive(true)
    .build()
    .unwrap();
  let mut screenshot_list = Vec::new();
  for path in get_files_with_regex(screenshots_dir, &valid_extensions)? {
    let metadata = path.metadata().unwrap();
    let modified_time = metadata.modified().unwrap();
    let timestamp = modified_time
      .duration_since(SystemTime::UNIX_EPOCH)
      .unwrap()
      .as_secs();
    screenshot_list.push(ScreenshotInfo {
      file_name: path.file_stem().unwrap().to_string_lossy().to_string(),
      file_path: path,
      time: timestamp,
    });
  }

  Ok(screenshot_list)
}

lazy_static! {
  static ref RENAME_LOCK: Mutex<()> = Mutex::new(());
  static ref RENAME_REGEX: Regex = RegexBuilder::new(r"^(.*?)(\.disabled)*$")
    .case_insensitive(true)
    .build()
    .unwrap();
}

#[tauri::command]
pub fn toggle_mod_by_extension(file_path: PathBuf, enable: bool) -> BGUMCLResult<()> {
  let _lock = RENAME_LOCK.lock().expect("Failed to acquire lock");
  if !file_path.is_file() {
    return Err(InstanceError::FileNotFoundError.into());
  }

  let file_name = file_path
    .file_name()
    .unwrap_or_default()
    .to_str()
    .unwrap_or_default();

  let new_name = if enable {
    if let Some(captures) = RENAME_REGEX.captures(file_name) {
      captures
        .get(1)
        .map(|m| m.as_str())
        .unwrap_or(file_name)
        .to_string()
    } else {
      file_name.to_string()
    }
  } else if RENAME_REGEX.is_match(file_name) {
    format!("{}.disabled", file_name)
  } else {
    file_name.to_string()
  };
  let new_path = file_path.with_file_name(new_name);

  if new_path != file_path {
    fs::rename(&file_path, &new_path)?;
  }

  Ok(())
}

#[tauri::command]
pub async fn retrieve_world_details(
  app: AppHandle,
  instance_id: String,
  world_name: String,
) -> BGUMCLResult<LevelData> {
  let worlds_dir =
    match get_instance_subdir_path_by_id(&app, &instance_id, &InstanceSubdirType::Saves) {
      Some(path) => path,
      None => return Err(InstanceError::WorldNotExistError.into()),
    };
  let level_path = worlds_dir.join(world_name).join("level.dat");
  if tokio::fs::metadata(&level_path).await.is_err() {
    return Err(InstanceError::LevelNotExistError.into());
  }
  if let Ok(level_data) = load_level_data_from_nbt(&level_path).await {
    Ok(level_data)
  } else {
    Err(InstanceError::LevelParseError.into())
  }
}

#[tauri::command]
pub fn create_launch_desktop_shortcut(
  app: AppHandle,
  instance_id: String,
  icon_src: String,
) -> BGUMCLResult<()> {
  let binding = app.state::<Mutex<HashMap<String, Instance>>>();
  let state = binding
    .lock()
    .map_err(|_| InstanceError::InstanceNotFoundByID)?;
  let instance = state
    .get(&instance_id)
    .ok_or(InstanceError::InstanceNotFoundByID)?;

  let name = instance.name.clone();
  let encoded_id = url::form_urlencoded::Serializer::new(String::new())
    .append_pair("id", &instance.id)
    .finish()
    .replace("+", "%20");
  let url = format!("bgumcl://launch?{}", encoded_id);

  #[cfg(any(target_os = "windows", target_os = "linux"))]
  let icon_path = {
    use crate::instance::helpers::misc::create_instance_shortcut_icon;
    create_instance_shortcut_icon(&app, instance, &icon_src).ok()
  };
  #[cfg(target_os = "macos")]
  let icon_path = {
    let _ = icon_src; // explicitly consume to avoid warning
    None
  };

  create_url_shortcut(&app, name, url, icon_path)
    .map_err(|_| InstanceError::ShortcutCreationFailed)?;

  Ok(())
}

#[tauri::command]
pub async fn create_instance(
  app: AppHandle,
  directory: GameDirectory,
  name: String,
  description: String,
  icon_src: String,
  game: GameClientResourceInfo,
  mod_loader: ModLoaderResourceInfo,
  optifine: Option<OptiFineResourceInfo>,
  modpack_path: Option<String>,
  mut is_install_fabric_api: Option<bool>,
  mut is_install_qf_api: Option<bool>,
  modpack_version: Option<String>,
) -> BGUMCLResult<()> {
  let client = app.state::<reqwest::Client>();
  let launcher_config_state = app.state::<Mutex<LauncherConfig>>();
  // Get priority list
  let (priority_list, auto_download_java) = {
    let launcher_config = launcher_config_state.lock()?;
    (
      get_source_priority_list(&launcher_config),
      launcher_config.general.functionality.auto_download_java,
    )
  };

  if name.is_empty() || !sanitize_filename::is_sanitized(&name) {
    return Err(InstanceError::InvalidNameError.into());
  }

  // An existing directory is never safe to recursively remove here: it may
  // contain saves, mods, or user-created files even when the two launcher
  // metadata files are missing. A cancelled creation is cleaned up only by
  // the exact in-memory ownership record after its task has stopped.
  let version_path = directory.dir.join("versions").join(&name);
  if version_path.exists() {
    return Err(InstanceError::ConflictNameError.into());
  }

  // Guard removes version_path on any early return (errors), fix #1105 #1310
  let dir_guard = RemoveDirGuard::new(version_path.clone());

  let optifine_info = optifine.as_ref().map(|info| OptiFine {
    filename: info.filename.clone(),
    version: format!("{}_{}", info.r#type, info.patch),
    status: ModLoaderStatus::NotDownloaded,
  });

  // Create instance config
  let instance = Instance {
    id: format!("{}:{}", directory.name, name.clone()),
    name: name.clone(),
    version: game.id.clone(),
    version_path: version_path.clone(),
    mod_loader: ModLoader {
      loader_type: mod_loader.loader_type,
      status: if matches!(
        mod_loader.loader_type,
        ModLoaderType::Unknown | ModLoaderType::Fabric | ModLoaderType::Quilt
      ) {
        ModLoaderStatus::Installed
      } else {
        ModLoaderStatus::NotDownloaded
      },
      version: mod_loader.version.clone(),
      branch: mod_loader.branch.clone(),
    },
    optifine: optifine_info,
    description,
    tag: None,
    icon_src,
    starred: false,
    play_time: 0,
    use_spec_game_config: false,
    spec_game_config: None,
    modpack_version: modpack_version.clone(),
    modpack_update_channel: None,
  };

  // Download version info through the same mainland mirror candidates as
  // ordinary Minecraft files. Read bytes first so a truncated/compressed
  // response can be retried on the next source instead of surfacing the raw
  // reqwest error "error decoding response body" to the UI.
  let mut version_info = None;
  let mut saw_version_parse_error = false;
  for candidate in crate::utils::web::minecraft_download_candidates(&game.url) {
    let response = match client
      .get(&candidate)
      .header("accept", "application/json")
      .header("accept-encoding", "identity")
      .send()
      .await
    {
      Ok(response) if response.status().is_success() => response,
      Ok(response) => {
        log::warn!(
          "Minecraft version metadata source returned {}: {}",
          response.status(),
          candidate
        );
        continue;
      }
      Err(error) => {
        log::warn!(
          "Minecraft version metadata request failed for {}: {}",
          candidate,
          error
        );
        continue;
      }
    };
    let body = match response.bytes().await {
      Ok(body) => body,
      Err(error) => {
        saw_version_parse_error = true;
        log::warn!(
          "Minecraft version metadata body failed for {}: {}",
          candidate,
          error
        );
        continue;
      }
    };
    match serde_json::from_slice::<McClientInfo>(&body) {
      Ok(parsed) => {
        version_info = Some(parsed);
        break;
      }
      Err(error) => {
        saw_version_parse_error = true;
        log::warn!(
          "Minecraft version metadata JSON parse failed for {} ({} bytes): {}",
          candidate,
          body.len(),
          error
        );
      }
    }
  }
  let mut version_info = version_info.ok_or(if saw_version_parse_error {
    InstanceError::ClientJsonParseError
  } else {
    InstanceError::NetworkError
  })?;

  version_info.id = name.clone();
  version_info.jar = Some(name.clone());

  // convert vanilla version info to vanilla patch
  let mut vanilla_patch = version_info.clone();
  vanilla_patch.id = "game".to_string();
  vanilla_patch.version = Some(game.id.clone());
  vanilla_patch.inherits_from = None;
  vanilla_patch.priority = Some(0);
  version_info.patches.push(vanilla_patch);

  let mut task_params = Vec::<PTaskParam>::new();

  // auto download recommended java if needed
  let mut java_version_to_download: Option<String> = None;
  if auto_download_java {
    let client_java_version = version_info
      .java_version
      .as_ref()
      .map_or(0i32, |version| version.major_version);

    if let Err(err) = select_java_runtime(&app, None, &instance, client_java_version).await
      && err.0 == LaunchError::NoSuitableJava.to_string()
    {
      let minimum_java_version = get_minimum_java_version_by_game(&app, &instance, false).await;
      let minimum_java_version = minimum_java_version.to_string();
      task_params.extend(build_mojang_java_download_params(&app, &minimum_java_version).await?);
      java_version_to_download = Some(minimum_java_version);
    }
  }

  // Download client (use task)
  let client_download_info = version_info
    .downloads
    .get("client")
    .ok_or(InstanceError::ClientJsonParseError)?;

  task_params.push(PTaskParam::Download(DownloadParam {
    src: Url::parse(&client_download_info.url.clone())
      .map_err(|_| InstanceError::ClientJsonParseError)?,
    dest: instance.version_path.join(format!("{}.jar", name)),
    filename: None,
    sha1: Some(client_download_info.sha1.clone()),
  }));
  let subdirs = get_instance_subdir_paths(
    &app,
    &instance,
    &[
      &InstanceSubdirType::Libraries,
      &InstanceSubdirType::Assets,
      &InstanceSubdirType::Mods,
    ],
  )
  .ok_or(InstanceError::InstanceNotFoundByID)?;
  let [libraries_dir, assets_dir, mods_dir] = subdirs.as_slice() else {
    return Err(InstanceError::InstanceNotFoundByID.into());
  };

  replace_native_libraries(&app, &mut version_info, &instance)
    .await
    .map_err(|_| InstanceError::ClientJsonParseError)?;

  // We only download libraries if they are invalid (not already downloaded)
  task_params.extend(
    get_invalid_library_files(priority_list[0], libraries_dir, &version_info, false).await?,
  );

  // We only download assets if they are invalid (not already downloaded)
  task_params
    .extend(get_invalid_assets(&app, &version_info, priority_list[0], assets_dir, false).await?);

  // When installing a modpack, skip auto-installing Fabric API / QFAPI to avoid
  // duplicates — the modpack manifest already specifies the exact version needed.
  if modpack_path.is_some() {
    is_install_fabric_api = Some(false);
    is_install_qf_api = Some(false);
  }

  // download loader (installer)
  if instance.mod_loader.loader_type != ModLoaderType::Unknown {
    install_mod_loader(
      app.clone(),
      &priority_list,
      &instance.version,
      &instance.mod_loader,
      libraries_dir.to_path_buf(),
      mods_dir.to_path_buf(),
      &mut version_info,
      &mut task_params,
      is_install_fabric_api,
      is_install_qf_api,
    )
    .await?;
  }

  if let Some(info) = optifine.as_ref() {
    download_optifine_installer(
      &app,
      &instance.version,
      info,
      libraries_dir.to_path_buf(),
      &mut task_params,
    )
    .await?;
  }

  // If modpack path is provided, install it
  if let Some(modpack_path) = modpack_path {
    let path = PathBuf::from(modpack_path);
    let file = fs::File::open(&path).map_err(|_| InstanceError::FileNotFoundError)?;
    task_params.extend(get_download_params(&app, &file, &version_path).await?);
    extract_overrides(&file, &version_path)?;
  }

  // Optionally skip first-screen options by adding options.txt.
  let (language, skip_first_screen_options) = {
    let launcher_config = launcher_config_state.lock()?;
    (
      launcher_config.general.general.language.clone(),
      launcher_config
        .general
        .functionality
        .skip_first_screen_options,
    )
  };
  if skip_first_screen_options
    && let Some(lang_code) = get_minecraft_lang_tag(&language, &instance.version, &app)
  {
    let options_path = get_instance_subdir_paths(&app, &instance, &[&InstanceSubdirType::Root])
      .ok_or(InstanceError::InstanceNotFoundByID)?[0]
      .join("options.txt");
    if !options_path.exists() {
      fs::write(options_path, format!("lang:{}\n", lang_code))
        .map_err(|_| InstanceError::FileCreationFailed)?;
    }
  }

  // Save the edited client json
  save_json_async(&version_info, &version_path.join(format!("{}.json", name))).await?;
  // Save the BGUMCL instance config json
  instance
    .save_json_cfg()
    .await
    .map_err(|_| InstanceError::FileCreationFailed)?;

  // Register the new instance in the in-memory state map so that subsequent
  // commands (e.g. set_github_modpack_update_channel) can find it immediately.
  {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let mut state = binding.lock()?;
    state.insert(instance.id.clone(), instance.clone());
  }

  // Persist all instance metadata before scheduling any asynchronous work. If
  // scheduling itself fails, the state entry and directory are rolled back by
  // this function's explicit error path and the guard below.
  let task_group = match java_version_to_download {
    Some(java_version) => format!("game-client-w-java?{}&{}", name, java_version),
    None => format!("game-client?{}", name),
  };
  let task_desc =
    match schedule_progressive_task_group(app.clone(), task_group, task_params, true).await {
      Ok(desc) => desc,
      Err(error) => {
        let binding = app.state::<Mutex<HashMap<String, Instance>>>();
        if let Ok(mut state) = binding.lock() {
          state.remove(&instance.id);
        }
        return Err(error);
      }
    };

  crate::instance::helpers::misc::register_pending_instance_creation(
    &app,
    task_desc.task_group,
    instance.clone(),
  )?;

  dir_guard.commit();
  Ok(())
}

#[tauri::command]
pub async fn continue_instance_creation(app: AppHandle, task_group: String) -> BGUMCLResult<()> {
  crate::instance::helpers::misc::continue_instance_creation(&app, &task_group).await
}

#[tauri::command]
pub async fn finish_mod_loader_install(app: AppHandle, instance_id: String) -> BGUMCLResult<()> {
  let instance = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let state = binding.lock()?;
    state
      .get(&instance_id)
      .ok_or(InstanceError::InstanceNotFoundByID)?
      .clone()
  };
  let client_info_dir = instance
    .version_path
    .join(format!("{}.json", instance.name));
  let client_info = load_json_async::<McClientInfo>(&client_info_dir).await?;

  match instance.mod_loader.status {
    // prevent duplicated installation
    ModLoaderStatus::DownloadFailed => {
      return Err(InstanceError::ProcessorExecutionFailed.into());
    }
    ModLoaderStatus::Installing => {
      return Err(InstanceError::InstallationDuplicated.into());
    }
    ModLoaderStatus::Downloading => {
      {
        let binding = app.state::<Mutex<HashMap<String, Instance>>>();
        let mut state = binding.lock()?;
        let instance = state
          .get_mut(&instance_id)
          .ok_or(InstanceError::InstanceNotFoundByID)?;
        instance.mod_loader.status = ModLoaderStatus::Installing;
      };

      let install_profile_dir = instance.version_path.join("install_profile.json");
      if install_profile_dir.exists() {
        let install_profile = load_json_async::<InstallProfile>(&install_profile_dir).await?;
        execute_processors(&app, &instance, &client_info, &install_profile).await?;
      }
    }
    _ => {}
  }

  let instance = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let mut state = binding.lock()?;
    let instance = state
      .get_mut(&instance_id)
      .ok_or(InstanceError::InstanceNotFoundByID)?;
    instance.mod_loader.status = ModLoaderStatus::Installed;
    instance.clone()
  };
  instance.save_json_cfg().await?;
  crate::instance::helpers::misc::continue_pending_instance_by_id(&app, &instance_id).await?;

  Ok(())
}

#[tauri::command]
pub async fn finish_optifine_loader_install(
  app: AppHandle,
  instance_id: String,
) -> BGUMCLResult<()> {
  let instance = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let state = binding.lock()?;
    state
      .get(&instance_id)
      .ok_or(InstanceError::InstanceNotFoundByID)?
      .clone()
  };
  let client_info_dir = instance
    .version_path
    .join(format!("{}.json", instance.name));
  let client_info = load_json_async::<McClientInfo>(&client_info_dir).await?;

  if let Some(optifine) = &instance.optifine {
    match optifine.status {
      // prevent duplicated installation
      ModLoaderStatus::DownloadFailed => {
        return Err(InstanceError::ProcessorExecutionFailed.into());
      }
      ModLoaderStatus::Installing => {
        return Err(InstanceError::InstallationDuplicated.into());
      }
      ModLoaderStatus::Downloading => {
        {
          let binding = app.state::<Mutex<HashMap<String, Instance>>>();
          let mut state = binding.lock()?;
          let instance = state
            .get_mut(&instance_id)
            .ok_or(InstanceError::InstanceNotFoundByID)?;
          instance.optifine.as_mut().unwrap().status = ModLoaderStatus::Installing;
        };
        finish_optifine_install(&app, &instance, &client_info).await?;
      }
      _ => {}
    }
  }
  let instance = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let mut state = binding.lock()?;
    let instance = state
      .get_mut(&instance_id)
      .ok_or(InstanceError::InstanceNotFoundByID)?;
    if let Some(optifine) = &mut instance.optifine {
      optifine.status = ModLoaderStatus::Installed;
    }
    instance.clone()
  };
  instance.save_json_cfg().await?;
  crate::instance::helpers::misc::continue_pending_instance_by_id(&app, &instance_id).await?;

  Ok(())
}

#[tauri::command]
pub async fn check_change_mod_loader_availablity(
  app: AppHandle,
  instance_id: String,
) -> BGUMCLResult<bool> {
  let instance = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let launcher_config_state = binding.lock()?;
    launcher_config_state
      .get(&instance_id)
      .ok_or(InstanceError::InstanceNotFoundByID)?
      .clone()
  };

  let json_path = instance
    .version_path
    .join(format!("{}.json", instance.name));
  if !json_path.exists() {
    return Err(InstanceError::NotSupportChangeModLoader.into());
  }

  let current_info: McClientInfo = load_json_async(&json_path)
    .await
    .map_err(|_| InstanceError::NotSupportChangeModLoader)?;

  if current_info.patches.is_empty() {
    return Err(InstanceError::NotSupportChangeModLoader.into());
  }

  Ok(true)
}

#[tauri::command]
pub async fn change_mod_loader(
  app: AppHandle,
  instance_id: String,
  new_mod_loader: ModLoaderResourceInfo,
  is_install_fabric_api: Option<bool>,
  is_install_qf_api: Option<bool>,
) -> BGUMCLResult<()> {
  let mut instance = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let state = binding.lock()?;
    state
      .get(&instance_id)
      .ok_or(InstanceError::InstanceNotFoundByID)?
      .clone()
  };
  let version_isolation = get_instance_game_config(&app, &instance).version_isolation;
  let priority_list = {
    let launcher_config_state = app.state::<Mutex<LauncherConfig>>();
    let launcher_config = launcher_config_state.lock()?;
    get_source_priority_list(&launcher_config)
  };
  let json_path = instance
    .version_path
    .join(format!("{}.json", instance.name));
  let current_info: McClientInfo = load_json_async(&json_path).await?;

  let game_version = instance.version.clone();
  let subdirs = get_instance_subdir_paths(
    &app,
    &instance,
    &[&InstanceSubdirType::Libraries, &InstanceSubdirType::Mods],
  )
  .ok_or(InstanceError::InstanceNotFoundByID)?;
  let [libraries_dir, mods_dir] = subdirs.as_slice() else {
    return Err(InstanceError::InstanceNotFoundByID.into());
  };

  // Remove Fabric API / QFAPI mods if switching from Fabric or Quilt modloader
  if matches!(
    instance.mod_loader.loader_type,
    ModLoaderType::Fabric | ModLoaderType::Quilt
  ) && instance.mod_loader.loader_type != new_mod_loader.loader_type
    && version_isolation
  {
    remove_fabric_api_mods(mods_dir).await?;
  }

  let mut version_info = current_info.clone();
  remove_mod_loader_from_client_info(&mut version_info, instance.mod_loader.loader_type);

  let mut modloader_task_params: Vec<PTaskParam> = Vec::new();

  let mod_loader = ModLoader {
    loader_type: new_mod_loader.loader_type,
    version: new_mod_loader.version.clone(),
    status: if matches!(
      new_mod_loader.loader_type,
      ModLoaderType::Unknown | ModLoaderType::Fabric | ModLoaderType::Quilt
    ) {
      ModLoaderStatus::Installed
    } else {
      ModLoaderStatus::NotDownloaded
    },
    branch: new_mod_loader.branch.clone(),
  };

  instance.mod_loader = mod_loader.clone();

  if mod_loader.loader_type != ModLoaderType::Unknown {
    install_mod_loader(
      app.clone(),
      &priority_list,
      &game_version,
      &mod_loader,
      libraries_dir.to_path_buf(),
      mods_dir.to_path_buf(),
      &mut version_info,
      &mut modloader_task_params,
      is_install_fabric_api,
      is_install_qf_api,
    )
    .await?;
  }

  if !modloader_task_params.is_empty() {
    schedule_progressive_task_group(
      app.clone(),
      format!(
        "change-mod-loader?{} {}",
        instance.mod_loader.loader_type, instance.mod_loader.version
      ),
      modloader_task_params,
      true,
    )
    .await?;
  }

  save_json_async(&version_info, &json_path).await?;
  instance
    .save_json_cfg()
    .await
    .map_err(|_| InstanceError::FileCreationFailed)?;

  Ok(())
}

#[tauri::command]
pub async fn remove_mod_loader(app: AppHandle, instance_id: String) -> BGUMCLResult<()> {
  let mut instance = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let state = binding.lock()?;
    state
      .get(&instance_id)
      .ok_or(InstanceError::InstanceNotFoundByID)?
      .clone()
  };
  let version_isolation = get_instance_game_config(&app, &instance).version_isolation;
  let json_path = instance
    .version_path
    .join(format!("{}.json", instance.name));
  let mut version_info: McClientInfo = load_json_async(&json_path).await?;

  let subdirs = get_instance_subdir_paths(&app, &instance, &[&InstanceSubdirType::Mods])
    .ok_or(InstanceError::InstanceNotFoundByID)?;
  let [mods_dir] = subdirs.as_slice() else {
    return Err(InstanceError::InstanceNotFoundByID.into());
  };

  if matches!(
    instance.mod_loader.loader_type,
    ModLoaderType::Fabric | ModLoaderType::Quilt
  ) && version_isolation
  {
    remove_fabric_api_mods(mods_dir).await?;
  }

  remove_mod_loader_from_client_info(&mut version_info, instance.mod_loader.loader_type);
  instance.mod_loader = ModLoader {
    loader_type: ModLoaderType::Unknown,
    version: String::new(),
    status: ModLoaderStatus::Installed,
    branch: None,
  };

  save_json_async(&version_info, &json_path).await?;
  instance
    .save_json_cfg()
    .await
    .map_err(|_| InstanceError::FileCreationFailed)?;

  Ok(())
}

#[tauri::command]
pub async fn change_optifine(
  app: AppHandle,
  instance_id: String,
  new_optifine: OptiFineResourceInfo,
) -> BGUMCLResult<()> {
  let mut instance = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let state = binding.lock()?;
    state
      .get(&instance_id)
      .ok_or(InstanceError::InstanceNotFoundByID)?
      .clone()
  };
  let subdirs = get_instance_subdir_paths(&app, &instance, &[&InstanceSubdirType::Libraries])
    .ok_or(InstanceError::InstanceNotFoundByID)?;
  let [libraries_dir] = subdirs.as_slice() else {
    return Err(InstanceError::InstanceNotFoundByID.into());
  };

  let optifine_info = OptiFine {
    filename: new_optifine.filename.clone(),
    version: format!("{}_{}", new_optifine.r#type, new_optifine.patch),
    status: ModLoaderStatus::NotDownloaded,
  };

  instance.optifine = Some(optifine_info);

  let mut optifine_task_params: Vec<PTaskParam> = Vec::new();
  download_optifine_installer(
    &app,
    &instance.version,
    &new_optifine,
    libraries_dir.to_path_buf(),
    &mut optifine_task_params,
  )
  .await?;

  if !optifine_task_params.is_empty() {
    schedule_progressive_task_group(
      app.clone(),
      format!("change-optifine?{}", new_optifine.filename),
      optifine_task_params,
      true,
    )
    .await?;
  }

  instance
    .save_json_cfg()
    .await
    .map_err(|_| InstanceError::FileCreationFailed)?;

  Ok(())
}

#[tauri::command]
pub async fn remove_optifine(app: AppHandle, instance_id: String) -> BGUMCLResult<()> {
  let mut instance = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let state = binding.lock()?;
    state
      .get(&instance_id)
      .ok_or(InstanceError::InstanceNotFoundByID)?
      .clone()
  };
  let json_path = instance
    .version_path
    .join(format!("{}.json", instance.name));
  let mut version_info: McClientInfo = load_json_async(&json_path).await?;

  remove_optifine_from_client_info(&mut version_info);
  instance.optifine = None;

  save_json_async(&version_info, &json_path).await?;
  instance
    .save_json_cfg()
    .await
    .map_err(|_| InstanceError::FileCreationFailed)?;

  Ok(())
}

#[tauri::command]
pub async fn retrieve_modpack_meta_info(
  app: AppHandle,
  path: String,
) -> BGUMCLResult<ModpackMetaInfo> {
  let path = PathBuf::from(path);
  let file = fs::File::open(&path).map_err(|_| InstanceError::FileNotFoundError)?;
  ModpackMetaInfo::from_archive(&app, &file).await
}

const MAX_NESTED_WANDA_MODPACK_SIZE: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug)]
enum WandaArchiveError {
  InvalidFormat,
  Io(std::io::Error),
}

impl From<std::io::Error> for WandaArchiveError {
  fn from(error: std::io::Error) -> Self {
    Self::Io(error)
  }
}

impl From<zip::result::ZipError> for WandaArchiveError {
  fn from(error: zip::result::ZipError) -> Self {
    match error {
      zip::result::ZipError::Io(error) => Self::Io(error),
      _ => Self::InvalidFormat,
    }
  }
}

fn wanda_archive_has_manifest(archive: &mut ZipArchive<fs::File>) -> bool {
  let has_modrinth_manifest = archive.by_name("modrinth.index.json").is_ok();
  let has_curseforge_manifest = archive.by_name("manifest.json").is_ok();
  has_modrinth_manifest || has_curseforge_manifest
}

fn safe_wanda_asset_name(asset_name: &str) -> String {
  let file_name = Path::new(asset_name)
    .file_name()
    .and_then(|name| name.to_str())
    .unwrap_or("wanda-server-modpack.zip");
  let sanitized = sanitize_filename::sanitize(file_name);
  if sanitized.is_empty() {
    "wanda-server-modpack.zip".to_string()
  } else {
    sanitized
  }
}

fn finalize_wanda_archive(
  downloaded_path: &Path,
  asset_name: &str,
  dest_dir: &Path,
) -> Result<PathBuf, WandaArchiveError> {
  let archive_file = fs::File::open(downloaded_path)?;
  let mut archive = ZipArchive::new(archive_file)?;
  let safe_asset_name = safe_wanda_asset_name(asset_name);

  if wanda_archive_has_manifest(&mut archive) {
    let destination = dest_dir.join(safe_asset_name);
    drop(archive);
    if destination.exists() {
      fs::remove_file(&destination)?;
    }
    fs::rename(downloaded_path, &destination)?;
    return Ok(destination);
  }

  let mut nested_mrpack_index: Option<usize> = None;
  for index in 0..archive.len() {
    let entry = archive.by_index(index)?;
    let is_mrpack = !entry.is_dir()
      && Path::new(entry.name())
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.eq_ignore_ascii_case("mrpack"))
        .unwrap_or(false);
    if is_mrpack {
      if nested_mrpack_index.is_some() {
        return Err(WandaArchiveError::InvalidFormat);
      }
      nested_mrpack_index = Some(index);
    }
  }

  let nested_mrpack_index = nested_mrpack_index.ok_or(WandaArchiveError::InvalidFormat)?;
  let outer_stem = Path::new(&safe_asset_name)
    .file_stem()
    .and_then(|stem| stem.to_str())
    .filter(|stem| !stem.is_empty())
    .unwrap_or("wanda-server-modpack");
  let nested_name = format!("{}.mrpack", outer_stem);
  let nested_destination = dest_dir.join(&nested_name);
  let nested_temp = dest_dir.join(format!("{}.download", nested_name));
  if nested_temp.exists() {
    fs::remove_file(&nested_temp)?;
  }

  {
    let mut nested_entry = archive.by_index(nested_mrpack_index)?;
    if nested_entry.size() == 0 || nested_entry.size() > MAX_NESTED_WANDA_MODPACK_SIZE {
      return Err(WandaArchiveError::InvalidFormat);
    }
    let mut nested_file = fs::File::create(&nested_temp)?;
    std::io::copy(&mut nested_entry, &mut nested_file)?;
    nested_file.sync_all()?;
  }
  drop(archive);

  let nested_archive_file = fs::File::open(&nested_temp)?;
  let mut nested_archive = match ZipArchive::new(nested_archive_file) {
    Ok(archive) => archive,
    Err(_) => {
      let _ = fs::remove_file(&nested_temp);
      return Err(WandaArchiveError::InvalidFormat);
    }
  };
  if !wanda_archive_has_manifest(&mut nested_archive) {
    drop(nested_archive);
    let _ = fs::remove_file(&nested_temp);
    return Err(WandaArchiveError::InvalidFormat);
  }
  drop(nested_archive);

  if nested_destination.exists() {
    fs::remove_file(&nested_destination)?;
  }
  fs::rename(&nested_temp, &nested_destination)?;
  let _ = fs::remove_file(downloaded_path);
  Ok(nested_destination)
}

fn wanda_release_asset(json: &serde_json::Value) -> Option<(String, String)> {
  let assets = json.get("assets")?.as_array()?;
  for extension in ["mrpack", "zip"] {
    for asset in assets {
      let Some(name) = asset.get("name").and_then(|value| value.as_str()) else {
        continue;
      };
      let Some(url) = asset
        .get("browser_download_url")
        .or_else(|| asset.get("download_url"))
        .or_else(|| asset.get("url"))
        .and_then(|value| value.as_str())
      else {
        continue;
      };
      let matches_extension = Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.eq_ignore_ascii_case(extension))
        .unwrap_or(false);
      if matches_extension && !url.contains("/archive/refs/tags/") {
        return Some((name.to_string(), url.to_string()));
      }
    }
  }
  None
}

#[cfg(test)]
mod wanda_modpack_tests {
  use super::*;
  use std::io::{Cursor, Write};
  use zip::ZipWriter;
  use zip::write::FileOptions;

  fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let cursor = Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    for (name, content) in entries {
      writer
        .start_file(*name, FileOptions::<()>::default())
        .unwrap();
      writer.write_all(content).unwrap();
    }
    writer.finish().unwrap().into_inner()
  }

  fn test_dir(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
      .duration_since(SystemTime::UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    std::env::temp_dir().join(format!(
      "bgumcl-wanda-{}-{}-{}",
      name,
      std::process::id(),
      nonce
    ))
  }

  #[test]
  fn extracts_and_validates_nested_mrpack() {
    let dir = test_dir("nested");
    fs::create_dir_all(&dir).unwrap();
    let inner = zip_bytes(&[("modrinth.index.json", br#"{"name":"test"}"#)]);
    let outer = zip_bytes(&[
      ("modpack.mrpack", inner.as_slice()),
      ("Plain Craft Launcher.exe", b"ignored"),
    ]);
    let downloaded = dir.join("wrapper.zip.download");
    fs::write(&downloaded, outer).unwrap();

    let result =
      finalize_wanda_archive(&downloaded, "Construct Technological Innovation.zip", &dir).unwrap();

    assert_eq!(
      result.file_name().and_then(|name| name.to_str()),
      Some("Construct Technological Innovation.mrpack")
    );
    assert!(!downloaded.exists());
    let result_file = fs::File::open(&result).unwrap();
    let mut archive = ZipArchive::new(result_file).unwrap();
    assert!(wanda_archive_has_manifest(&mut archive));
    drop(archive);
    fs::remove_dir_all(&dir).unwrap();
  }

  #[test]
  fn ignores_generated_source_archives() {
    let release = serde_json::json!({
      "assets": [
        {
          "name": "1.1.0.zip",
          "browser_download_url": "https://gitee.com/example/repo/archive/refs/tags/1.1.0.zip"
        },
        {
          "name": "Construct Technological Innovation.zip",
          "browser_download_url": "https://gitee.com/example/repo/releases/download/1.1.0/pack.zip"
        }
      ]
    });

    let asset = wanda_release_asset(&release).unwrap();
    assert_eq!(asset.0, "Construct Technological Innovation.zip");
  }
}

#[tauri::command]
pub async fn download_wanda_modpack(app: AppHandle) -> BGUMCLResult<String> {
  // Use the dedicated download client (browser User-Agent + long timeout) so
  // proxies do not throttle us and large files are not cut off.
  let client = crate::tasks::download::download_client().clone();

  // GitHub file proxies are not API proxies: they may return an HTML page or
  // a compressed error body with HTTP 200. Query the real APIs only, then use
  // the Gitee release attachment as the file-level mainland fallback.
  let api_candidates = [
    "https://gitee.com/api/v5/repos/Muzimimiao/BBGU-Minecraft-sever/releases/latest",
    "https://api.github.com/repos/Muzimi-ciallo/BBGU-Minecraft-sever/releases/latest",
  ];

  let mut release_assets: Vec<(String, String)> = Vec::new();
  let mut best_release_version: Option<semver::Version> = None;
  let mut saw_release_response = false;
  for api_url in &api_candidates {
    let response = match client
      .get(*api_url)
      .header("accept", "application/vnd.github+json")
      .header("x-github-api-version", "2022-11-28")
      .send()
      .await
    {
      Ok(response) => response,
      Err(error) => {
        log::warn!(
          "Wanda release API request failed for {}: {:?}",
          api_url,
          error
        );
        continue;
      }
    };
    if !response.status().is_success() {
      log::warn!(
        "Wanda release API returned {} for {}",
        response.status(),
        api_url
      );
      continue;
    }
    let body = match response.bytes().await {
      Ok(body) => body,
      Err(error) => {
        log::warn!("Wanda release API body failed for {}: {:?}", api_url, error);
        continue;
      }
    };
    match serde_json::from_slice::<serde_json::Value>(&body) {
      Ok(parsed)
        if parsed
          .get("assets")
          .and_then(|value| value.as_array())
          .is_some() =>
      {
        saw_release_response = true;
        let Some(asset) = wanda_release_asset(&parsed) else {
          log::warn!("Wanda release has no importable asset: {}", api_url);
          continue;
        };
        let release_version = parsed
          .get("tag_name")
          .and_then(|value| value.as_str())
          .and_then(|value| semver::Version::parse(value.trim_start_matches('v')).ok());

        match (&best_release_version, &release_version) {
          (Some(best), Some(current)) if current < best => continue,
          (Some(best), Some(current)) if current > best => {
            release_assets.clear();
            best_release_version = Some(current.clone());
          }
          (None, Some(current)) => {
            release_assets.clear();
            best_release_version = Some(current.clone());
          }
          (Some(_), None) => continue,
          _ => {}
        }
        if !release_assets.iter().any(|(_, url)| url == &asset.1) {
          release_assets.push(asset);
        }
      }
      Ok(_) => {
        log::warn!(
          "Wanda release API returned JSON without assets: {}",
          api_url
        );
      }
      Err(error) => {
        // Some acceleration endpoints return a successful HTML error page.
        // Treat it as a bad candidate and continue with the next source.
        log::warn!(
          "Wanda release API returned non-JSON from {}: {:?}",
          api_url,
          error
        );
      }
    }
  }

  // Use every API that reports the newest release. This keeps the real Gitee
  // attachment first while retaining GitHub proxy/direct fallbacks when the
  // Gitee file service is temporarily unavailable.
  let (name, candidates): (String, Vec<String>) = if !release_assets.is_empty() {
    let name = release_assets[0].0.clone();
    let mut candidates = Vec::new();
    for (_, url) in release_assets {
      for candidate in crate::utils::web::gh_proxy_candidates(&url) {
        if !candidates.contains(&candidate) {
          candidates.push(candidate);
        }
      }
    }
    (name, candidates)
  } else if saw_release_response {
    return Err(InstanceError::ModpackManifestParseError.into());
  } else {
    // GitHub API unreachable: use the known mrpack on the Gitee release.
    let name = "Fabulously.Optimized-v6.5.0.mrpack".to_string();
    let candidates = vec![format!(
      "https://gitee.com/Muzimimiao/BBGU-Minecraft-sever/releases/download/1.0.0/{}",
      name
    )];
    (name, candidates)
  };

  let dest_dir = app
    .path()
    .resolve::<PathBuf>("Download".into(), BaseDirectory::AppCache)
    .map_err(|_| InstanceError::NetworkError)?;
  fs::create_dir_all(&dest_dir).map_err(|_| InstanceError::FileCreationFailed)?;
  let temp_dest = dest_dir.join(format!("{}.download", safe_wanda_asset_name(&name)));
  let mut saw_invalid_archive = false;
  let mut saw_file_error = false;
  for candidate in &candidates {
    let Ok(resp) = client.get(candidate).send().await else {
      continue;
    };
    if !resp.status().is_success() {
      log::warn!(
        "Wanda asset source returned {}: {}",
        resp.status(),
        candidate
      );
      continue;
    }
    let _ = tokio::fs::remove_file(&temp_dest).await;
    let mut file = match tokio::fs::File::create(&temp_dest).await {
      Ok(f) => f,
      Err(_) => return Err(InstanceError::FileCreationFailed.into()),
    };
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;
    let mut stream = resp.bytes_stream();
    let mut ok = true;
    while let Some(chunk) = stream.next().await {
      match chunk {
        Ok(bytes) => {
          if file.write_all(&bytes).await.is_err() {
            ok = false;
            break;
          }
        }
        Err(_) => {
          ok = false;
          break;
        }
      }
    }
    if ok && file.flush().await.is_ok() {
      drop(file);
      let archive_path = temp_dest.clone();
      let asset_name = name.clone();
      let archive_dest_dir = dest_dir.clone();
      match tokio::task::spawn_blocking(move || {
        finalize_wanda_archive(&archive_path, &asset_name, &archive_dest_dir)
      })
      .await
      {
        Ok(Ok(destination)) => {
          return Ok(destination.to_string_lossy().to_string());
        }
        Ok(Err(WandaArchiveError::InvalidFormat)) => {
          saw_invalid_archive = true;
          log::warn!("Wanda asset is not a valid modpack archive: {}", candidate);
        }
        Ok(Err(WandaArchiveError::Io(error))) => {
          saw_file_error = true;
          log::warn!(
            "Wanda asset finalization failed for {}: {:?}",
            candidate,
            error
          );
        }
        Err(error) => {
          log::warn!(
            "Wanda asset validation task failed for {}: {:?}",
            candidate,
            error
          );
        }
      }
    }
    let _ = tokio::fs::remove_file(&temp_dest).await;
  }
  let _ = tokio::fs::remove_file(&temp_dest).await;
  if saw_invalid_archive {
    return Err(InstanceError::ModpackManifestParseError.into());
  }
  if saw_file_error {
    return Err(InstanceError::FileCreationFailed.into());
  }
  Err(InstanceError::NetworkError.into())
}
#[tauri::command]
pub fn add_custom_instance_icon(
  app: AppHandle,
  instance_id: String,
  source_src: String,
) -> BGUMCLResult<()> {
  let version_path = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let state = binding.lock()?;
    let instance = state
      .get(&instance_id)
      .ok_or(InstanceError::InstanceNotFoundByID)?;
    instance.version_path.clone()
  };

  let source_path = Path::new(&source_src);
  if !source_path.exists() || !source_path.is_file() {
    return Err(InstanceError::FileNotFoundError.into());
  }

  let dest_path = Path::new(&version_path).join("icon");
  fs::copy(source_path, &dest_path)?;

  Ok(())
}

#[tauri::command]
pub async fn retrieve_exportable_file_list(
  app: AppHandle,
  instance_id: String,
) -> BGUMCLResult<ModpackFileList> {
  let instance = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let state = binding.lock()?;
    state
      .get(&instance_id)
      .ok_or(InstanceError::InstanceNotFoundByID)?
      .clone()
  };
  tokio::task::spawn_blocking(move || list_files(&instance)).await?
}

#[tauri::command]
pub async fn export_modpack(
  app: AppHandle,
  instance_id: String,
  save_path: String,
  options: ExportModpackOptions,
  files: Vec<String>,
) -> BGUMCLResult<()> {
  let instance = {
    let binding = app.state::<Mutex<HashMap<String, Instance>>>();
    let state = binding.lock()?;
    state
      .get(&instance_id)
      .ok_or(InstanceError::InstanceNotFoundByID)?
      .clone()
  };
  validate_export_options(&instance, &options)?;

  let base_path = instance.version_path.clone();

  let mut selected_files = Vec::new();
  for rel in files {
    let full = base_path.join(&rel);
    if tokio::fs::try_exists(&full).await.unwrap_or(false) {
      selected_files.push((rel, full));
    }
  }

  if selected_files.is_empty() {
    return Err(InstanceError::ModpackManifestParseError.into());
  }

  let export_bundle = build_export_bundle(&app, &instance, &options, &selected_files).await?;

  create_modpack_zip(&save_path, export_bundle)
    .await
    .map_err(|_| InstanceError::ZipFileProcessFailed)?;

  Ok(())
}
