use quartz_nbt::io::{Flavor, read_nbt};
use quartz_nbt::{NbtCompound, NbtTag};
use sjmcl_types::error::{BGUMCLError, BGUMCLResult};
use std::io::ErrorKind;
use std::path::Path;
use uuid::Uuid;

use crate::instance::models::misc::InstanceError;
use crate::instance::models::world::{WorldDetails, WorldInfo};
use crate::utils::nbt::nbt_to_display_value;

pub async fn load_world_info_from_dir(
  path: &Path,
  has_difficulty_support: bool,
) -> BGUMCLResult<WorldInfo> {
  // Both formats keep the world-list metadata in level.dat.
  let level = load_level_nbt(&path.join("level.dat")).await?;
  let (last_played, difficulty, gamemode) =
    level_data_to_world_info(level.get::<_, &NbtCompound>("Data")?)?;

  Ok(WorldInfo {
    name: path
      .file_name()
      .and_then(|n| n.to_str())
      .unwrap_or("")
      .to_string(),
    last_played_at: last_played,
    difficulty: has_difficulty_support.then_some(difficulty),
    gamemode,
    icon_src: path.join("icon.png"),
    dir_path: path.to_path_buf(),
  })
}

async fn read_nbt_file(path: &Path) -> BGUMCLResult<Option<NbtCompound>> {
  let bytes = match tokio::fs::read(path).await {
    Ok(bytes) => bytes,
    Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
    Err(err) => return Err(err.into()),
  };
  Ok(Some(
    read_nbt(&mut bytes.as_slice(), Flavor::GzCompressed)?.0,
  ))
}

async fn load_level_nbt(path: &Path) -> BGUMCLResult<NbtCompound> {
  let level = read_nbt_file(path)
    .await
    .map_err(|err| {
      log::warn!(
        "Failed to read world level data at {}: {:?}",
        path.display(),
        err
      );
      InstanceError::LevelParseError
    })?
    .ok_or(InstanceError::LevelNotExistError)?;
  level.get::<_, &NbtCompound>("Data").map_err(|err| {
    log::warn!("Invalid world level data at {}: {}", path.display(), err);
    InstanceError::LevelParseError
  })?;
  Ok(level)
}

pub async fn load_world_data_from_dir(path: &Path) -> BGUMCLResult<WorldDetails> {
  // Both formats use level.dat's Data compound; 26.1+ moves some fields to external files.
  // https://minecraft.wiki/w/Java_Edition_level_format
  let level = load_level_nbt(&path.join("level.dat")).await?;
  let data = level.get::<_, &NbtCompound>("Data")?;

  // Optional metadata files in the 26.1+ layout.
  let mut files = vec![
    "data/minecraft/world_gen_settings.dat".to_string(),
    "data/minecraft/weather.dat".to_string(),
    "data/minecraft/game_rules.dat".to_string(),
    "data/minecraft/wandering_trader.dat".to_string(),
    "dimensions/minecraft/overworld/data/minecraft/world_border.dat".to_string(),
  ];

  // 26.1+ replaces Player with singleplayer_uuid, referencing players/data/<uuid>.dat.
  // https://minecraft.wiki/w/Player.dat_format
  if !data.contains_key("Player")
    && let Some(tag) = data.inner().get("singleplayer_uuid")
  {
    match tag {
      NbtTag::IntArray(parts) if parts.len() == 4 => {
        // Preserve the bits of the four signed UUID words, most significant first.
        let most = ((parts[0] as u32 as u64) << 32) | parts[1] as u32 as u64;
        let least = ((parts[2] as u32 as u64) << 32) | parts[3] as u32 as u64;
        files.push(format!(
          "players/data/{}.dat",
          Uuid::from_u64_pair(most, least)
        ));
      }
      _ => log::warn!("Invalid singleplayer_uuid in {}", path.display()),
    }
  }

  let mut details = WorldDetails::from([(
    "level.dat".to_string(),
    nbt_to_display_value(NbtTag::Compound(level)),
  )]);
  for file in files {
    let file_path = path.join(&file);
    match read_nbt_file(&file_path).await {
      Ok(Some(data)) => {
        details.insert(file, nbt_to_display_value(NbtTag::Compound(data)));
      }
      Ok(None) if file.starts_with("players/") => {
        log::warn!("Missing world player data at {}", file_path.display());
      }
      Ok(None) => {}
      Err(err) => log::warn!(
        "Failed to read world data at {}: {:?}",
        file_path.display(),
        err
      ),
    }
  }
  Ok(details)
}

fn level_data_to_world_info(data: &NbtCompound) -> BGUMCLResult<(i64, String, String)> {
  // Prefer 26.1+ difficulty_settings; fall back to the legacy top-level tags.
  let settings = data
    .inner()
    .get("difficulty_settings")
    .map(<&NbtCompound>::try_from)
    .transpose()?;
  let hardcore = settings
    .and_then(|s| s.inner().get("hardcore"))
    .or_else(|| data.inner().get("hardcore"))
    .map(bool::try_from)
    .transpose()?
    .unwrap_or(false);

  const DIFFICULTIES: [&str; 4] = ["peaceful", "easy", "normal", "hard"];
  let difficulty = if let Some(tag) = settings.and_then(|s| s.inner().get("difficulty")) {
    let value = <&str>::try_from(tag)?;
    if !DIFFICULTIES.contains(&value) {
      return Err(BGUMCLError(format!("Invalid world difficulty: {}", value)));
    }
    value
  } else {
    let value = data
      .inner()
      .get("Difficulty")
      .map(u8::try_from)
      .transpose()?
      .unwrap_or(2);
    DIFFICULTIES
      .get(value as usize)
      .copied()
      .ok_or_else(|| BGUMCLError(format!("Invalid world difficulty: {}", value)))?
  };

  const GAMEMODES: [&str; 4] = ["survival", "creative", "adventure", "spectator"];
  // Worlds predating game modes have no GameType and are always survival.
  let game_type = data
    .inner()
    .get("GameType")
    .map(i32::try_from)
    .transpose()?
    .unwrap_or(0);
  let gamemode = GAMEMODES
    .get(game_type as usize)
    .ok_or_else(|| BGUMCLError(format!("Invalid world game type: {}", game_type)))?;

  Ok((
    data.get::<_, i64>("LastPlayed")? / 1000,
    if hardcore { "hardcore" } else { difficulty }.to_string(),
    gamemode.to_string(),
  ))
}

#[cfg(test)]
mod tests {
  use crate::instance::helpers::world::{
    level_data_to_world_info, load_world_data_from_dir, load_world_info_from_dir,
  };
  use quartz_nbt::io::write_nbt;
  use quartz_nbt::{NbtCompound, NbtTag};
  use uuid::Uuid;

  struct TestWorld(std::path::PathBuf);

  impl TestWorld {
    fn new() -> Self {
      let path = std::env::temp_dir().join(format!("bgumcl-world-test-{}", Uuid::new_v4()));
      std::fs::create_dir_all(&path).unwrap();
      Self(path)
    }

    fn write(&self, file: &str, data: &NbtCompound) {
      let path = self.0.join(file);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      let mut bytes = Vec::new();
      write_nbt(&mut bytes, None, data, Flavor::GzCompressed).unwrap();
      std::fs::write(path, bytes).unwrap();
    }

    fn level(&self, data: NbtCompound) {
      let mut root = NbtCompound::new();
      root.insert("Data", data);
      self.write("level.dat", &root);
    }
  }

  impl Drop for TestWorld {
    fn drop(&mut self) {
      let _ = std::fs::remove_dir_all(&self.0);
    }
  }

  fn legacy_data() -> NbtCompound {
    let mut data = NbtCompound::new();
    data.insert("LastPlayed", 1_700_000_000_000_i64);
    data.insert("GameType", 1_i32);
    data.insert("Difficulty", 2_i8);
    data.insert("hardcore", 0_i8);
    data
  }

  #[tokio::test]
  async fn reads_legacy_worlds_without_requiring_player_fields() {
    let world = TestWorld::new();
    world.level(legacy_data());
    let info = load_world_info_from_dir(&world.0, true).await.unwrap();
    assert_eq!(info.last_played_at, 1_700_000_000);
    assert_eq!(info.difficulty.as_deref(), Some("normal"));
    assert_eq!(info.gamemode, "creative");
    let details = load_world_data_from_dir(&world.0).await.unwrap();
    assert_eq!(details.len(), 1);
    assert_eq!(details["level.dat"]["Data"]["GameType"], 1);
  }

  #[tokio::test]
  async fn reads_26x_external_metadata_and_signed_uuid_words() {
    let world = TestWorld::new();
    let mut data = legacy_data();
    let mut difficulty = NbtCompound::new();
    difficulty.insert("difficulty", "hard");
    difficulty.insert("hardcore", 1_i8);
    data.insert("difficulty_settings", difficulty);
    data.insert(
      "singleplayer_uuid",
      NbtTag::IntArray(vec![-1, 0, i32::MIN, 1]),
    );
    world.level(data);
    let mut metadata = NbtCompound::new();
    metadata.insert("seed", i64::MAX);
    world.write("data/minecraft/world_gen_settings.dat", &metadata);
    let player = "players/data/ffffffff-0000-0000-8000-000000000001.dat";
    world.write(player, &metadata);
    let info = load_world_info_from_dir(&world.0, true).await.unwrap();
    assert_eq!(info.difficulty.as_deref(), Some("hardcore"));
    let details = load_world_data_from_dir(&world.0).await.unwrap();
    assert_eq!(details.len(), 3);
    assert_eq!(details[player]["seed"], i64::MAX.to_string());
  }

  #[tokio::test]
  async fn corrupt_optional_metadata_does_not_hide_the_world() {
    let world = TestWorld::new();
    world.level(legacy_data());
    let path = world.0.join("data/minecraft/weather.dat");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"invalid nbt").unwrap();
    assert_eq!(load_world_data_from_dir(&world.0).await.unwrap().len(), 1);
    assert!(
      load_world_info_from_dir(&world.0, false)
        .await
        .unwrap()
        .difficulty
        .is_none()
    );
  }

  #[tokio::test]
  async fn missing_and_invalid_level_files_return_errors() {
    let world = TestWorld::new();
    assert!(load_world_data_from_dir(&world.0).await.is_err());
    world.write("level.dat", &NbtCompound::new());
    assert!(load_world_data_from_dir(&world.0).await.is_err());
    let mut data = legacy_data();
    data.insert("GameType", -1_i32);
    assert!(level_data_to_world_info(&data).is_err());
    data.insert("GameType", 0_i32);
    data.insert("Difficulty", 5_i8);
    assert!(level_data_to_world_info(&data).is_err());
  }
}
