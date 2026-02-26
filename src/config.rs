use serde::{Deserialize, Serialize};
use tg::TelegramDestination;
use v_utils::{
	macros::{LiveSettings, MyConfigPrimitives, Settings},
	trades::Timeframe,
};

/// Initialize the data directory (call after config is loaded)
pub fn init_data_dir() {
	use crate::server::DATA_DIR;
	// xdg_state_dir! creates the directory and returns the path
	let data_dir = DATA_DIR.get_or_init(|| v_utils::xdg_state_dir!(""));
	tracing::info!("Data directory ready: {}", data_dir.display());
}
/// Metadata for discovered topics in forum groups
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TopicsMetadata {
	/// Maps group_id -> { topic_id -> custom_name }
	/// If a topic has a custom name, use it; otherwise fall back to topic_{id}
	pub groups: std::collections::BTreeMap<u64, GroupMetadata>,
}
impl TopicsMetadata {
	pub fn file_path() -> std::path::PathBuf {
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
				tracing::warn!("Failed to read topics_metadata.json: {e}");
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
		self.groups.get(&group_id).and_then(|g| g.name.clone()).unwrap_or_else(|| format!("group_{group_id}"))
	}

	/// Get the display name for a topic
	pub fn topic_name(&self, group_id: u64, topic_id: u64) -> String {
		self.groups
			.get(&group_id)
			.and_then(|g| g.topics.get(&topic_id).cloned())
			.unwrap_or_else(|| format!("topic_{topic_id}"))
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
		self.groups.entry(group_id).or_default().topics.entry(topic_id).or_insert_with(|| format!("topic_{topic_id}"));
	}
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct GroupMetadata {
	/// Custom name for the group (optional, falls back to group_id)
	pub name: Option<String>,
	/// Maps topic_id -> custom_name
	pub topics: std::collections::BTreeMap<u64, String>,
}
#[derive(Clone, Debug, Default, LiveSettings, MyConfigPrimitives, Settings)]
pub(crate) struct AppConfig {
	#[settings(default = 8123)]
	pub localhost_port: u16,
	#[settings(default = 1000)]
	pub max_messages_per_chat: usize,
	/// How far back to pull TODOs from channel messages (default: 1 week)
	pub pull_todos_over: Option<Timeframe>,
	/// Named group destinations
	#[primitives(skip)]
	pub groups: Option<std::collections::HashMap<String, TelegramDestination>>,
	/// Interval for periodic pull from Telegram (default: 1m)
	pub pull_interval: Option<Timeframe>,
	/// Interval for alerts channel polling (default: 1m)
	pub alerts_interval: Option<Timeframe>,
	/// Channel to monitor for alerts (shows desktop notifications for unread messages)
	#[private_value]
	pub alerts_channel: Option<TelegramDestination>,
	/// Maximum number of messages to print without confirmation prompt (default: 64)
	pub print_confirm_threshold: Option<usize>,
	/// Telegram API ID from https://my.telegram.org/
	pub api_id: Option<i32>,
	/// Telegram API hash (can be { env = "VAR_NAME" })
	pub api_hash: Option<String>,
	/// Phone number for Telegram auth (can be { env = "VAR_NAME" })
	pub phone: Option<String>,
	/// Telegram username for session file naming
	pub username: Option<String>,
}

impl AppConfig {
	pub fn pull_todos_over(&self) -> Timeframe {
		self.pull_todos_over.unwrap_or_else(|| Timeframe::from(&"1w"))
	}

	pub fn pull_interval(&self) -> Timeframe {
		self.pull_interval.unwrap_or_else(|| Timeframe::from(&"1m"))
	}

	pub fn alerts_interval(&self) -> Timeframe {
		self.alerts_interval.unwrap_or_else(|| Timeframe::from(&"1m"))
	}

	pub fn print_confirm_threshold(&self) -> usize {
		self.print_confirm_threshold.unwrap_or(64)
	}

	/// Get unique group IDs from all group destinations
	pub fn forum_group_ids(&self) -> Vec<u64> {
		let mut group_ids = std::collections::HashSet::new();
		if let Some(groups) = &self.groups {
			for dest in groups.values() {
				if let Some(id) = dest.group_id() {
					group_ids.insert(id);
				}
			}
		}
		group_ids.into_iter().collect()
	}

	/// Resolve a group name to TelegramDestination
	#[cfg(test)]
	pub fn resolve_group(&self, name: &str) -> Option<&TelegramDestination> {
		self.groups.as_ref()?.get(name)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

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
localhost_port = 8123
max_messages_per_chat = 500

[groups]
general = "-1002244305221"
work = "-1002244305221/3"
alerts = "-1001234567890"
"#;

		let config: AppConfig = toml::from_str(toml_str).unwrap();

		assert_eq!(config.localhost_port, 8123);
		assert_eq!(config.max_messages_per_chat, 500);

		// Check group resolution
		let general = config.resolve_group("general").unwrap();
		assert_eq!(general.as_group_topic(), Some((2244305221, 1)));

		let work = config.resolve_group("work").unwrap();
		assert_eq!(work.as_group_topic(), Some((2244305221, 3)));

		let alerts = config.resolve_group("alerts").unwrap();
		assert_eq!(alerts.as_group_topic(), Some((1234567890, 1)));

		// Check derived forum_group_ids
		let mut group_ids = config.forum_group_ids();
		group_ids.sort();
		assert_eq!(group_ids, vec![1234567890, 2244305221]);
	}

	#[test]
	fn test_config_defaults() {
		// Note: defaults only work via Settings derive, not direct toml parsing
		let toml_str = r#"
localhost_port = 8080
max_messages_per_chat = 1000
"#;

		let config: AppConfig = toml::from_str(toml_str).unwrap();

		// Values should match what was set
		assert_eq!(config.max_messages_per_chat, 1000);
		// No groups = empty forum_group_ids
		assert!(config.forum_group_ids().is_empty());
	}
}
