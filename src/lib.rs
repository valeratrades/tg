// tg library - Telegram forum group syncing tool

use eyre::{Result, eyre};
use serde::{Deserialize, Deserializer, Serialize, de};

/// Identifier for a Telegram user or bot.
///
/// Can be either a username (with or without `@` prefix) or a numeric user ID.
/// Unlike channels/groups, user IDs don't use the `-100` prefix.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub enum Username {
	/// Username like "@username" or "username"
	At(String),
	/// Numeric user ID (positive integer, no prefix)
	Id(u64),
}

impl Username {
	/// Get the identifier as a string suitable for Telegram API.
	/// For usernames, ensures `@` prefix. For IDs, returns the numeric string.
	pub fn api_param(&self) -> String {
		match self {
			Self::At(name) =>
				if name.starts_with('@') {
					name.clone()
				} else {
					format!("@{name}")
				},
			Self::Id(id) => id.to_string(),
		}
	}

	/// Get the numeric ID if available
	pub fn numeric_id(&self) -> Option<u64> {
		match self {
			Self::At(_) => None,
			Self::Id(id) => Some(*id),
		}
	}

	/// Get the username string if available (without `@` prefix)
	pub fn name(&self) -> Option<&str> {
		match self {
			Self::At(name) => Some(name.trim_start_matches('@')),
			Self::Id(_) => None,
		}
	}
}

impl<'de> Deserialize<'de> for Username {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>, {
		#[derive(Deserialize)]
		#[serde(untagged)]
		enum Helper {
			Unsigned(u64),
			Signed(i64),
			String(String),
		}

		let helper = Helper::deserialize(deserializer)?;
		match helper {
			Helper::Unsigned(id) => Ok(Username::Id(id)),
			Helper::Signed(id) =>
				if id < 0 {
					Err(de::Error::custom("User IDs cannot be negative"))
				} else {
					Ok(Username::Id(id as u64))
				},
			Helper::String(s) => parse_username(&s).map_err(de::Error::custom),
		}
	}
}

fn parse_username(s: &str) -> Result<Username> {
	let trimmed = s.trim();

	// Check if it's a username (starts with @)
	if trimmed.starts_with('@') {
		return Ok(Username::At(trimmed.to_string()));
	}

	// Try to parse as numeric ID first
	if let Ok(id) = trimmed.parse::<u64>() {
		return Ok(Username::Id(id));
	}

	// If it looks like a username (alphanumeric + underscores), treat as username
	if trimmed.chars().all(|c| c.is_alphanumeric() || c == '_') {
		return Ok(Username::At(trimmed.to_string()));
	}

	Err(eyre!("Invalid username: {}", trimmed))
}

/// Top-level identifier for a Telegram channel or group.
///
/// This type is specifically for channels/supergroups which use the `-100` prefix
/// in their full chat IDs.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub enum TopLevelId {
	/// Username like "@channel_name" or "channel_name"
	AtName(String),
	/// Numeric ID (without -100 prefix)
	Id(u64),
}

impl TopLevelId {
	/// Get the chat_id parameter value for Telegram API
	pub fn chat_id_param(&self) -> String {
		match self {
			Self::AtName(name) =>
				if name.starts_with('@') {
					name.clone()
				} else {
					format!("@{name}")
				},
			Self::Id(id) => format!("-100{id}"),
		}
	}

	/// Get the numeric ID if available
	pub fn numeric_id(&self) -> Option<u64> {
		match self {
			Self::AtName(_) => None,
			Self::Id(id) => Some(*id),
		}
	}
}

impl<'de> Deserialize<'de> for TopLevelId {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>, {
		#[derive(Deserialize)]
		#[serde(untagged)]
		enum Helper {
			Unsigned(u64),
			Signed(i64),
			String(String),
		}

		let helper = Helper::deserialize(deserializer)?;
		match helper {
			Helper::Unsigned(id) => Ok(TopLevelId::Id(id)),
			Helper::Signed(id) => {
				// Strip -100 prefix if present
				let s = id.to_string();
				let stripped = s.trim_start_matches("-100").trim_start_matches('-');
				stripped.parse::<u64>().map(TopLevelId::Id).map_err(de::Error::custom)
			}
			Helper::String(s) => parse_top_level_id(&s).map_err(de::Error::custom),
		}
	}
}

fn parse_top_level_id(s: &str) -> Result<TopLevelId> {
	let trimmed = s.trim();

	// Check if it's a username (starts with @ or contains only letters/underscores)
	if trimmed.starts_with('@') || trimmed.chars().all(|c| c.is_alphabetic() || c == '_') {
		return Ok(TopLevelId::AtName(trimmed.to_string()));
	}

	// Try to parse as numeric ID, stripping -100 prefix if present
	let stripped = trimmed.trim_start_matches("-100").trim_start_matches('-');
	stripped.parse::<u64>().map(TopLevelId::Id).map_err(|e| eyre!("Failed to parse ID: {}", e))
}

/// Telegram destination for sending messages.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub enum TelegramDestination {
	/// A channel (broadcast-only)
	Channel(TopLevelId),
	/// A group/supergroup without topic
	Group(TopLevelId),
	/// A forum group with a specific topic
	GroupTopic { group: TopLevelId, topic_id: u64 },
}

impl TelegramDestination {
	/// Get the parameters for Telegram API calls
	pub fn destination_params(&self) -> Vec<(&str, String)> {
		match self {
			Self::Channel(id) | Self::Group(id) => {
				vec![("chat_id", id.chat_id_param())]
			}
			Self::GroupTopic { group, topic_id } => {
				vec![("chat_id", group.chat_id_param()), ("message_thread_id", topic_id.to_string())]
			}
		}
	}

	/// Get the group/channel ID (without -100 prefix) if available
	pub fn group_id(&self) -> Option<u64> {
		match self {
			Self::Channel(id) | Self::Group(id) | Self::GroupTopic { group: id, .. } => id.numeric_id(),
		}
	}

	/// Get the topic ID if this is a GroupTopic destination
	pub fn topic_id(&self) -> Option<u64> {
		match self {
			Self::GroupTopic { topic_id, .. } => Some(*topic_id),
			_ => None,
		}
	}

	/// Get group_id and topic_id as a tuple (topic_id defaults to 1 for General topic)
	pub fn as_group_topic(&self) -> Option<(u64, u64)> {
		match self {
			Self::Channel(id) | Self::Group(id) => id.numeric_id().map(|gid| (gid, 1)),
			Self::GroupTopic { group, topic_id } => group.numeric_id().map(|gid| (gid, *topic_id)),
		}
	}

	/// Calculate the full chat ID that Telegram uses (with -100 prefix)
	pub fn telegram_chat_id(&self) -> Option<i64> {
		self.group_id().map(|id| format!("-100{id}").parse().unwrap())
	}
}

impl Default for TelegramDestination {
	fn default() -> Self {
		Self::Group(TopLevelId::Id(0))
	}
}

impl<'de> Deserialize<'de> for TelegramDestination {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>, {
		#[derive(Deserialize)]
		#[serde(untagged)]
		enum Helper {
			// Object form: { group = ..., topic_id = ... } or { channel = ... }
			GroupTopic { group: TopLevelId, topic_id: u64 },
			Channel { channel: TopLevelId },
			Group { group: TopLevelId },
			// Inline forms
			Unsigned(u64),
			Signed(i64),
			String(String),
		}

		let helper = Helper::deserialize(deserializer)?;
		match helper {
			Helper::GroupTopic { group, topic_id } => Ok(TelegramDestination::GroupTopic { group, topic_id }),
			Helper::Channel { channel } => Ok(TelegramDestination::Channel(channel)),
			Helper::Group { group } => Ok(TelegramDestination::Group(group)),
			Helper::Unsigned(id) => Ok(TelegramDestination::Group(TopLevelId::Id(id))),
			Helper::Signed(id) => {
				let s = id.to_string();
				let stripped = s.trim_start_matches("-100").trim_start_matches('-');
				let parsed_id = stripped.parse::<u64>().map_err(de::Error::custom)?;
				Ok(TelegramDestination::Group(TopLevelId::Id(parsed_id)))
			}
			Helper::String(s) => parse_telegram_destination_str(&s).map_err(de::Error::custom),
		}
	}
}

impl std::str::FromStr for TelegramDestination {
	type Err = eyre::Report;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		parse_telegram_destination_str(s)
	}
}

fn parse_telegram_destination_str(s: &str) -> Result<TelegramDestination> {
	let trimmed = s.trim();

	// Check for "id/topic_id" format
	if let Some((id_str, topic_str)) = trimmed.split_once('/') {
		let group = parse_top_level_id(id_str)?;
		let topic_id = topic_str.parse::<u64>().map_err(|e| eyre!("Failed to parse topic ID: {}", e))?;
		return Ok(TelegramDestination::GroupTopic { group, topic_id });
	}

	// Otherwise it's a group (we can't distinguish channel vs group from string alone)
	let id = parse_top_level_id(trimmed)?;
	Ok(TelegramDestination::Group(id))
}

/// Calculate the full chat ID that Telegram uses (with -100 prefix for channels/supergroups)
pub fn telegram_chat_id(id: u64) -> i64 {
	format!("-100{id}").parse().unwrap()
}

/// Extract the group ID from a Telegram chat_id (strips -100 prefix)
pub fn extract_group_id(chat_id: i64) -> Option<u64> {
	let s = chat_id.to_string();
	if let Some(stripped) = s.strip_prefix("-100") {
		stripped.parse().ok()
	} else if chat_id > 0 {
		Some(chat_id as u64)
	} else {
		// Negative but doesn't start with -100
		s[1..].parse().ok()
	}
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;

	use insta::assert_debug_snapshot;
	use serde_json::from_str;

	use super::*;

	#[test]
	fn test_deserialize_group_numeric() {
		let json = r#""2244305221""#;
		let dest: TelegramDestination = from_str(json).unwrap();
		assert_debug_snapshot!(dest, @r###"
  Group(
      Id(
          2244305221,
      ),
  )
  "###);

		// All these should parse to the same group
		let cases = [r#"2244305221"#, r#""-1002244305221""#, r#"-1002244305221"#];
		for case in cases {
			let parsed: TelegramDestination = from_str(case).unwrap();
			assert_eq!(parsed.group_id(), Some(2244305221));
		}
	}

	#[test]
	fn test_deserialize_group_topic() {
		let json = r#""2244305221/7""#;
		let dest: TelegramDestination = from_str(json).unwrap();
		assert_debug_snapshot!(dest, @r###"
  GroupTopic {
      group: Id(
          2244305221,
      ),
      topic_id: 7,
  }
  "###);
	}

	#[test]
	fn test_deserialize_username() {
		let json = r#""WatchingTT""#;
		let dest: TelegramDestination = from_str(json).unwrap();
		assert_debug_snapshot!(dest, @r###"
  Group(
      AtName(
          "WatchingTT",
      ),
  )
  "###);

		let json_with_at = r#""@WatchingTT""#;
		let dest_with_at: TelegramDestination = from_str(json_with_at).unwrap();
		assert_debug_snapshot!(dest_with_at, @r###"
  Group(
      AtName(
          "@WatchingTT",
      ),
  )
  "###);
	}

	#[test]
	fn test_deserialize_object_form() {
		// Group with topic as object
		let toml_str = r#"
[dest]
group = 2244305221
topic_id = 7
"#;
		#[derive(Deserialize)]
		struct Wrapper {
			dest: TelegramDestination,
		}
		let w: Wrapper = toml::from_str(toml_str).unwrap();
		assert_eq!(w.dest.as_group_topic(), Some((2244305221, 7)));
	}

	#[test]
	fn test_deserialize_config_map() {
		let toml_str = r#"
wtt = "2244305221"
journal = "-1002244305222"
alerts = "2244305223/7"
watching = "WatchingTT"
"#;

		let config_groups: BTreeMap<String, TelegramDestination> = toml::from_str(toml_str).unwrap();

		assert_debug_snapshot!(config_groups, @r###"
  {
      "alerts": GroupTopic {
          group: Id(
              2244305223,
          ),
          topic_id: 7,
      },
      "journal": Group(
          Id(
              2244305222,
          ),
      ),
      "watching": Group(
          AtName(
              "WatchingTT",
          ),
      ),
      "wtt": Group(
          Id(
              2244305221,
          ),
      ),
  }
  "###);
	}

	#[test]
	fn test_destination_params() {
		let group = TelegramDestination::Group(TopLevelId::Id(123));
		assert_eq!(group.destination_params(), vec![("chat_id", "-100123".to_string())]);

		let group_topic = TelegramDestination::GroupTopic {
			group: TopLevelId::Id(123),
			topic_id: 7,
		};
		assert_eq!(group_topic.destination_params(), vec![("chat_id", "-100123".to_string()), ("message_thread_id", "7".to_string())]);

		let username = TelegramDestination::Group(TopLevelId::AtName("test".to_string()));
		assert_eq!(username.destination_params(), vec![("chat_id", "@test".to_string())]);

		let username_at = TelegramDestination::Group(TopLevelId::AtName("@test".to_string()));
		assert_eq!(username_at.destination_params(), vec![("chat_id", "@test".to_string())]);
	}

	#[test]
	fn test_as_group_topic() {
		let group = TelegramDestination::Group(TopLevelId::Id(123));
		assert_eq!(group.as_group_topic(), Some((123, 1)));

		let group_topic = TelegramDestination::GroupTopic {
			group: TopLevelId::Id(123),
			topic_id: 7,
		};
		assert_eq!(group_topic.as_group_topic(), Some((123, 7)));

		let username = TelegramDestination::Group(TopLevelId::AtName("test".to_string()));
		assert_eq!(username.as_group_topic(), None);
	}

	#[test]
	fn test_telegram_chat_id() {
		assert_eq!(telegram_chat_id(2244305221), -1002244305221);
		assert_eq!(telegram_chat_id(1), -1001);
		assert_eq!(telegram_chat_id(123456789), -100123456789);
	}

	#[test]
	fn test_extract_group_id() {
		assert_eq!(extract_group_id(-1002244305221), Some(2244305221));
		assert_eq!(extract_group_id(-1001), Some(1));
		assert_eq!(extract_group_id(-100123456789), Some(123456789));
		assert_eq!(extract_group_id(123456), Some(123456));
		assert_eq!(extract_group_id(-123456), Some(123456));
	}

	#[test]
	fn test_telegram_chat_id_roundtrip() {
		let original_id: u64 = 2244305221;
		let tg_id = telegram_chat_id(original_id);
		let extracted = extract_group_id(tg_id);
		assert_eq!(extracted, Some(original_id));
	}

	// Username tests
	#[test]
	fn test_username_numeric() {
		let id: Username = from_str("123456789").unwrap();
		assert_eq!(id, Username::Id(123456789));
		assert_eq!(id.numeric_id(), Some(123456789));
		assert_eq!(id.name(), None);
		assert_eq!(id.api_param(), "123456789");
	}

	#[test]
	fn test_username_at() {
		let id: Username = from_str(r#""johndoe""#).unwrap();
		assert_eq!(id, Username::At("johndoe".to_string()));
		assert_eq!(id.numeric_id(), None);
		assert_eq!(id.name(), Some("johndoe"));
		assert_eq!(id.api_param(), "@johndoe");
	}

	#[test]
	fn test_username_with_at_prefix() {
		let id: Username = from_str(r#""@johndoe""#).unwrap();
		assert_eq!(id, Username::At("@johndoe".to_string()));
		assert_eq!(id.numeric_id(), None);
		assert_eq!(id.name(), Some("johndoe"));
		assert_eq!(id.api_param(), "@johndoe");
	}

	#[test]
	fn test_username_with_numbers() {
		// Usernames can contain numbers but can't be all numbers
		let id: Username = from_str(r#""john123""#).unwrap();
		assert_eq!(id, Username::At("john123".to_string()));
	}

	#[test]
	fn test_username_rejects_negative() {
		// User IDs can't be negative (unlike channel IDs with -100 prefix)
		let result: Result<Username, _> = from_str("-123456");
		assert!(result.is_err());
	}

	#[test]
	fn test_username_from_toml() {
		#[derive(Deserialize)]
		struct Config {
			user: Username,
		}

		let toml_str = r#"user = 123456"#;
		let config: Config = toml::from_str(toml_str).unwrap();
		assert_eq!(config.user, Username::Id(123456));

		let toml_str = r#"user = "testuser""#;
		let config: Config = toml::from_str(toml_str).unwrap();
		assert_eq!(config.user, Username::At("testuser".to_string()));
	}
}
