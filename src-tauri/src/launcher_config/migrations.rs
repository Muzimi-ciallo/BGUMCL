use serde::Deserialize;
use serde::de::Deserializer;
use serde_json::Value;

use crate::launcher_config::models::AppearanceBackgroundConfig;

// Migrate old built-in wallpaper choices to the new default preset.
const LEGACY_BUILT_IN_BACKGROUNDS: &[&str] = &["%built-in:Jokull", "%built-in:GNLXC"];

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct BackgroundPayload {
  choice: String,
  random_custom: bool,
  auto_darken: bool,
}

pub fn deserialize_background<'de, D>(
  deserializer: D,
) -> Result<AppearanceBackgroundConfig, D::Error>
where
  D: Deserializer<'de>,
{
  let mut payload = BackgroundPayload::deserialize(deserializer)?;

  if LEGACY_BUILT_IN_BACKGROUNDS.contains(&payload.choice.as_str()) {
    payload.choice = "%built-in:BGUMCL".to_string();
    payload.auto_darken = false;
  }

  Ok(AppearanceBackgroundConfig {
    choice: payload.choice,
    random_custom: payload.random_custom,
    auto_darken: payload.auto_darken,
  })
}

// Deserializing discover sources from old and new formats.
// Migrated from Vec<String> to Vec<(String, bool)> with default enabled=true
const MUA_NEWS_SOURCE: &str = "https://www.mualliance.cn/news/api-sjmcl/article";
const UNIVERSITY_NEWS_SOURCE: &str = "https://api.jsumc.fun/sjmcl?type=tribee&src=535";
const LEGACY_NEWS_SOURCES: &[&str] = &[
  "https://mc.sjtu.cn/api-sjmcl/article",
  "https://mc.sjtu.cn/api-sjmcl/article/mua",
];

pub fn default_discover_sources() -> Vec<(String, bool)> {
  vec![
    (MUA_NEWS_SOURCE.to_string(), true),
    (UNIVERSITY_NEWS_SOURCE.to_string(), true),
  ]
}

fn migrate_discover_sources(mut sources: Vec<(String, bool)>) -> Vec<(String, bool)> {
  let Some(index) = sources
    .iter()
    .position(|(url, _)| LEGACY_NEWS_SOURCES.contains(&url.as_str()))
  else {
    return sources;
  };
  let enabled = sources
    .iter()
    .any(|(url, enabled)| LEGACY_NEWS_SOURCES.contains(&url.as_str()) && *enabled);
  sources.retain(|(url, _)| !LEGACY_NEWS_SOURCES.contains(&url.as_str()));
  if !sources.iter().any(|(url, _)| url == MUA_NEWS_SOURCE) {
    sources.insert(
      index.min(sources.len()),
      (MUA_NEWS_SOURCE.to_string(), enabled),
    );
  }
  if !sources.iter().any(|(url, _)| url == UNIVERSITY_NEWS_SOURCE) {
    sources.push((UNIVERSITY_NEWS_SOURCE.to_string(), enabled));
  }
  sources
}

pub fn deserialize_discover_sources<'de, D>(
  deserializer: D,
) -> Result<Vec<(String, bool)>, D::Error>
where
  D: Deserializer<'de>,
{
  let value = match Value::deserialize(deserializer) {
    Ok(value) => value,
    Err(_) => return Ok(Vec::default()),
  };

  let items = match value.as_array() {
    Some(items) => items,
    None => return Ok(Vec::default()),
  };

  Ok(migrate_discover_sources(
    items
      .iter()
      .filter_map(|item| match item {
        Value::String(url) => Some((url.to_string(), true)),
        Value::Array(tuple) if tuple.len() == 2 => {
          let url = tuple.first()?.as_str()?;
          let enabled = tuple.get(1)?.as_bool()?;
          Some((url.to_string(), enabled))
        }
        _ => None,
      })
      .collect(),
  ))
}

#[cfg(test)]
mod tests {
  use crate::launcher_config::migrations::{
    LEGACY_NEWS_SOURCES, MUA_NEWS_SOURCE, UNIVERSITY_NEWS_SOURCE, deserialize_discover_sources,
    migrate_discover_sources,
  };
  use serde::Deserialize;

  #[derive(Deserialize)]
  struct Config {
    #[serde(deserialize_with = "deserialize_discover_sources")]
    sources: Vec<(String, bool)>,
  }

  #[test]
  fn legacy_news_sources_migrate_without_losing_custom_sources() {
    let config: Config = serde_json::from_value(serde_json::json!({
      "sources": [
        [LEGACY_NEWS_SOURCES[0], false],
        ["https://example.com/feed", false],
        [LEGACY_NEWS_SOURCES[1], true]
      ]
    }))
    .unwrap();
    assert_eq!(
      config.sources,
      vec![
        (MUA_NEWS_SOURCE.to_string(), true),
        ("https://example.com/feed".to_string(), false),
        (UNIVERSITY_NEWS_SOURCE.to_string(), true),
      ]
    );
    assert_eq!(
      migrate_discover_sources(config.sources.clone()),
      config.sources
    );
  }

  #[test]
  fn migration_preserves_disabled_and_removed_sources() {
    let migrated = migrate_discover_sources(vec![(LEGACY_NEWS_SOURCES[0].to_string(), false)]);
    assert!(migrated.iter().all(|(_, enabled)| !enabled));
    assert!(migrate_discover_sources(Vec::new()).is_empty());
    let custom = vec![("https://example.com/feed".to_string(), false)];
    assert_eq!(migrate_discover_sources(custom.clone()), custom);
  }

  #[test]
  fn string_sources_and_existing_new_source_settings_are_preserved() {
    let config: Config = serde_json::from_value(serde_json::json!({
      "sources": [LEGACY_NEWS_SOURCES[0], "https://example.com/feed"]
    }))
    .unwrap();
    assert_eq!(config.sources.len(), 3);
    let migrated = migrate_discover_sources(vec![
      (LEGACY_NEWS_SOURCES[0].to_string(), true),
      (MUA_NEWS_SOURCE.to_string(), false),
      (UNIVERSITY_NEWS_SOURCE.to_string(), false),
    ]);
    assert_eq!(migrated.len(), 2);
    assert!(migrated.iter().all(|(_, enabled)| !enabled));
  }
}
