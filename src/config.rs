use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};
use v_utils::macros::MyConfigPrimitives;

use crate::server::DATA_DIR;

#[derive(Clone, Debug, MyConfigPrimitives, derive_new::new)]
pub struct AppConfig {
	/// Forum groups (supergroups with topics enabled) - all topics will be auto-discovered
	pub forum_groups: Vec<u64>,
	pub localhost_port: u16,
	#[new(default)]
	pub max_messages_per_chat: Option<usize>,
}

impl Default for AppConfig {
	fn default() -> Self {
		Self {
			forum_groups: Vec::new(),
			localhost_port: 0,
			max_messages_per_chat: Some(1000),
		}
	}
}

impl AppConfig {
	pub fn max_messages_per_chat(&self) -> usize {
		self.max_messages_per_chat.unwrap_or(1000)
	}

	pub fn read(path: &Path) -> Result<Self, config::ConfigError> {
		info!("Reading config from: {}", path.display());

		if !path.exists() {
			error!("Config file does not exist at path: {}", path.display());
			return Err(config::ConfigError::Message(format!("Config file not found: {}", path.display())));
		}

		debug!("Building config from file");
		let builder = config::Config::builder().add_source(config::File::with_name(&format!("{}", path.display())));

		let settings: config::Config = match builder.build() {
			Ok(s) => {
				debug!("Successfully built config");
				s
			}
			Err(e) => {
				error!("Failed to build config: {}", e);
				return Err(e);
			}
		};

		let settings: Self = match settings.try_deserialize::<Self>() {
			Ok(s) => {
				info!("Successfully deserialized config with {} forum groups", s.forum_groups.len());
				debug!("Forum groups: {:?}", s.forum_groups);
				s
			}
			Err(e) => {
				error!("Failed to deserialize config: {}", e);
				return Err(e);
			}
		};

		let data_dir_path = std::env::var("XDG_DATA_HOME")
			.map(PathBuf::from)
			.unwrap_or_else(|_| {
				warn!("XDG_DATA_HOME not set, this will panic");
				PathBuf::from("") // This will cause unwrap to panic
			})
			.join("tg");

		info!("Initializing data directory: {}", data_dir_path.display());
		let data_dir = DATA_DIR.get_or_init(|| data_dir_path);

		match std::fs::create_dir_all(data_dir) {
			Ok(_) => {
				info!("Data directory ready: {}", data_dir.display());
			}
			Err(e) => {
				error!("Failed to create data directory '{}': {}", data_dir.display(), e);
				return Err(config::ConfigError::Foreign(Box::new(e)));
			}
		}

		Ok(settings)
	}
}

/// Metadata for discovered topics in forum groups
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TopicsMetadata {
	/// Maps group_id -> { topic_id -> custom_name }
	/// If a topic has a custom name, use it; otherwise fall back to topic_{id}
	pub groups: std::collections::BTreeMap<u64, GroupMetadata>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct GroupMetadata {
	/// Custom name for the group (optional, falls back to group_id)
	pub name: Option<String>,
	/// Maps topic_id -> custom_name
	pub topics: std::collections::BTreeMap<u64, String>,
}

impl TopicsMetadata {
	pub fn file_path() -> PathBuf {
		v_utils::xdg_state_file!("topics_metadata.json")
	}

	pub fn load() -> Self {
		let path = Self::file_path();
		if !path.exists() {
			return Self::default();
		}

		match std::fs::read_to_string(&path) {
			Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
			Err(e) => {
				warn!("Failed to read topics_metadata.json: {}", e);
				Self::default()
			}
		}
	}

	pub fn save(&self) -> eyre::Result<()> {
		let path = Self::file_path();
		let content = serde_json::to_string_pretty(self)?;
		std::fs::write(&path, content)?;
		Ok(())
	}

	/// Get the display name for a group
	pub fn group_name(&self, group_id: u64) -> String {
		self.groups.get(&group_id).and_then(|g| g.name.clone()).unwrap_or_else(|| format!("group_{}", group_id))
	}

	/// Get the display name for a topic
	pub fn topic_name(&self, group_id: u64, topic_id: u64) -> String {
		self.groups
			.get(&group_id)
			.and_then(|g| g.topics.get(&topic_id).cloned())
			.unwrap_or_else(|| format!("topic_{}", topic_id))
	}

	/// Set a custom name for a group
	pub fn set_group_name(&mut self, group_id: u64, name: String) {
		self.groups.entry(group_id).or_default().name = Some(name);
	}

	/// Set a custom name for a topic
	pub fn set_topic_name(&mut self, group_id: u64, topic_id: u64, name: String) {
		self.groups.entry(group_id).or_default().topics.insert(topic_id, name);
	}

	/// Ensure a topic is registered (creates entry with default name if not exists)
	pub fn ensure_topic(&mut self, group_id: u64, topic_id: u64) {
		self.groups.entry(group_id).or_default().topics.entry(topic_id).or_insert_with(|| format!("topic_{}", topic_id));
	}
}

/// Calculate the full chat ID that Telegram uses (with -100 prefix for channels/supergroups)
pub fn telegram_chat_id(id: u64) -> i64 {
	format!("-100{}", id).parse().unwrap()
}

/// Extract the group ID from a Telegram chat_id (strips -100 prefix)
pub fn extract_group_id(chat_id: i64) -> Option<u64> {
	let s = chat_id.to_string();
	if s.starts_with("-100") {
		s[4..].parse().ok()
	} else if chat_id > 0 {
		Some(chat_id as u64)
	} else {
		// Negative but doesn't start with -100
		s[1..].parse().ok()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_telegram_chat_id() {
		assert_eq!(telegram_chat_id(2244305221), -1002244305221);
		assert_eq!(telegram_chat_id(1), -1001);
		assert_eq!(telegram_chat_id(123456789), -100123456789);
	}

	#[test]
	fn test_extract_group_id() {
		// Standard supergroup format with -100 prefix
		assert_eq!(extract_group_id(-1002244305221), Some(2244305221));
		assert_eq!(extract_group_id(-1001), Some(1));
		assert_eq!(extract_group_id(-100123456789), Some(123456789));

		// Positive IDs (user chats)
		assert_eq!(extract_group_id(123456), Some(123456));

		// Regular negative group IDs (old style)
		assert_eq!(extract_group_id(-123456), Some(123456));
	}

	#[test]
	fn test_telegram_chat_id_roundtrip() {
		let original_id: u64 = 2244305221;
		let telegram_id = telegram_chat_id(original_id);
		let extracted = extract_group_id(telegram_id);
		assert_eq!(extracted, Some(original_id));
	}

	#[test]
	fn test_topics_metadata_defaults() {
		let metadata = TopicsMetadata::default();

		// Unknown group should return default name
		assert_eq!(metadata.group_name(12345), "group_12345");

		// Unknown topic should return default name
		assert_eq!(metadata.topic_name(12345, 7), "topic_7");
	}

	#[test]
	fn test_topics_metadata_custom_names() {
		let mut metadata = TopicsMetadata::default();

		// Set custom group name
		metadata.set_group_name(12345, "my_group".to_string());
		assert_eq!(metadata.group_name(12345), "my_group");

		// Set custom topic name
		metadata.set_topic_name(12345, 7, "alerts".to_string());
		assert_eq!(metadata.topic_name(12345, 7), "alerts");

		// Other topic in same group still uses default
		assert_eq!(metadata.topic_name(12345, 8), "topic_8");

		// Other group still uses default
		assert_eq!(metadata.group_name(99999), "group_99999");
	}

	#[test]
	fn test_topics_metadata_ensure_topic() {
		let mut metadata = TopicsMetadata::default();

		// Ensure topic creates entry with default name
		metadata.ensure_topic(12345, 7);
		assert_eq!(metadata.topic_name(12345, 7), "topic_7");

		// Ensure topic doesn't overwrite existing custom name
		metadata.set_topic_name(12345, 7, "custom".to_string());
		metadata.ensure_topic(12345, 7);
		assert_eq!(metadata.topic_name(12345, 7), "custom");

		// Group entry is created
		assert!(metadata.groups.contains_key(&12345));
		assert!(metadata.groups.get(&12345).unwrap().topics.contains_key(&7));
	}

	#[test]
	fn test_topics_metadata_serialization() {
		let mut metadata = TopicsMetadata::default();
		metadata.set_group_name(12345, "test_group".to_string());
		metadata.set_topic_name(12345, 1, "general".to_string());
		metadata.set_topic_name(12345, 7, "alerts".to_string());

		// Serialize to JSON
		let json = serde_json::to_string(&metadata).unwrap();

		// Deserialize back
		let restored: TopicsMetadata = serde_json::from_str(&json).unwrap();

		assert_eq!(restored.group_name(12345), "test_group");
		assert_eq!(restored.topic_name(12345, 1), "general");
		assert_eq!(restored.topic_name(12345, 7), "alerts");
	}

	#[test]
	fn test_config_deserialization() {
		let toml_str = r#"
forum_groups = [2244305221, 1234567890]
localhost_port = 8123
max_messages_per_chat = 500
"#;

		let config: AppConfig = toml::from_str(toml_str).unwrap();

		assert_eq!(config.forum_groups, vec![2244305221, 1234567890]);
		assert_eq!(config.localhost_port, 8123);
		assert_eq!(config.max_messages_per_chat(), 500);
	}

	#[test]
	fn test_config_defaults() {
		let toml_str = r#"
forum_groups = []
localhost_port = 8080
"#;

		let config: AppConfig = toml::from_str(toml_str).unwrap();

		// max_messages_per_chat should use default
		assert_eq!(config.max_messages_per_chat(), 1000);
	}
}
