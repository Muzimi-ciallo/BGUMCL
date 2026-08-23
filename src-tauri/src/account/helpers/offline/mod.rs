pub mod yggdrasil_server;

use rand::seq::IteratorRandom;
use sjmcl_types::error::BGUMCLResult;
use strum::IntoEnumIterator;
use tauri::AppHandle;
use uuid::Uuid;

use crate::account::models::{
  AccountError, PlayerInfo, PlayerType, PresetRole, SkinModel, Texture, TextureType,
};
use crate::utils::fs::get_app_resource_filepath;
use crate::utils::image::load_image_from_dir;

pub fn load_preset_skin(app: &AppHandle, preset_role: PresetRole) -> BGUMCLResult<Vec<Texture>> {
  let model = if preset_role == PresetRole::Alex {
    SkinModel::Slim
  } else {
    SkinModel::Default
  };

  // Try to load the bundled preset skin PNG. If it is missing for any reason
  // (e.g. portable assets not extracted), fall back to a solid-color placeholder
  // so offline account creation never fails and never returns an empty texture list.
  let texture_img = get_app_resource_filepath(app, &format!("assets/skins/{}.png", preset_role))
    .ok()
    .and_then(|path| load_image_from_dir(&path))
    .unwrap_or_else(|| image::RgbaImage::from_pixel(64, 64, image::Rgba([143, 168, 168, 255])));

  Ok(vec![Texture {
    texture_type: TextureType::Skin,
    image: texture_img.into(),
    model,
    preset: Some(preset_role),
  }])
}

pub async fn login(
  app: &AppHandle,
  username: String,
  raw_uuid: String,
) -> BGUMCLResult<PlayerInfo> {
  let name_with_prefix = format!("OfflinePlayer:{}", username);
  let uuid = if let Ok(id) = Uuid::parse_str(&raw_uuid) {
    id
  } else {
    if !raw_uuid.is_empty() {
      // user uses custom UUID, but it's invalid
      return Err(AccountError::Invalid)?;
    }
    Uuid::new_v5(&Uuid::NAMESPACE_URL, name_with_prefix.as_bytes())
  };
  let preset_role = PresetRole::iter()
    .choose(&mut rand::rng())
    .unwrap_or(PresetRole::Steve);

  Ok(
    PlayerInfo {
      id: "".to_string(),
      name: username.clone(),
      uuid,
      player_type: PlayerType::Offline,
      auth_account: None,
      auth_server_url: None,
      access_token: None,
      access_token_expires: None,
      refresh_token: None,
      textures: load_preset_skin(app, preset_role)?,
    }
    .with_generated_id(),
  )
}
