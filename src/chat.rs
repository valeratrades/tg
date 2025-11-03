use std::str::FromStr;

use eyre::{Result, eyre};
use serde::{Deserialize, Deserializer, Serialize, de};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(untagged)]
/// Doesn't store "-100" prefix for numeric IDs
pub enum TelegramDestination {
	ChannelExactUid(u64),
	ChannelUsername(String),
	Group { id: u64, thread_id: u64 },
}

impl TelegramDestination {
	pub fn destination_params(&self) -> Vec<(&str, String)> {
		match self {
			Self::ChannelExactUid(chat_id) => vec![("chat_id", format!("-100{chat_id}"))],
			Self::ChannelUsername(username) => {
				let username = if username.starts_with('@') { username.clone() } else { format!("@{}", username) };
				vec![("chat_id", username)]
			}
			Self::Group { id, thread_id } => vec![("chat_id", format!("-100{id}")), ("message_thread_id", thread_id.to_string())],
		}
	}
}

impl Default for TelegramDestination {
	fn default() -> Self {
		Self::ChannelExactUid(0)
	}
}

impl<'de> Deserialize<'de> for TelegramDestination {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>, {
		#[derive(Deserialize)]
		#[serde(untagged)]
		enum TelegramDestinationHelper {
			Channel(u64),
			Group { id: u64, thread_id: u64 },
			String(String),
			Signed64(i64),
		}

		let helper = TelegramDestinationHelper::deserialize(deserializer)?;
		match helper {
			TelegramDestinationHelper::Channel(id) => Ok(TelegramDestination::ChannelExactUid(id)),
			TelegramDestinationHelper::Group { id, thread_id } => Ok(TelegramDestination::Group { id, thread_id }),
			TelegramDestinationHelper::String(s) => parse_telegram_destination_str(&s).map_err(de::Error::custom),
			TelegramDestinationHelper::Signed64(id) => parse_telegram_destination_str(&id.to_string()).map_err(de::Error::custom),
		}
	}
}

impl std::str::FromStr for TelegramDestination {
	type Err = eyre::Report;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		parse_telegram_destination_str(s)
	}
}

fn parse_telegram_destination_str(s: &str) -> Result<TelegramDestination, eyre::Report> {
	fn parse_chat_id(mut s: &str) -> Result<u64, eyre::Report> {
		s = s.trim_start_matches("-100");
		s.parse::<u64>().map_err(|e| eyre!("Failed to parse chat ID: {}", e))
	}

	// Check if it's a username (starts with @ or contains only letters/underscores)
	let trimmed = s.trim();
	if trimmed.starts_with('@') || (!trimmed.contains('/') && trimmed.chars().all(|c| c.is_alphabetic() || c == '_')) {
		return Ok(TelegramDestination::ChannelUsername(trimmed.to_string()));
	}

	if let Some((id_str, thread_id_str)) = s.split_once('/') {
		let id = parse_chat_id(id_str)?;
		let thread_id = u64::from_str(thread_id_str).map_err(|e| eyre!("Failed to parse thread ID: {}", e))?;
		Ok(TelegramDestination::Group { id, thread_id })
	} else {
		let id = parse_chat_id(s)?;
		Ok(TelegramDestination::ChannelExactUid(id))
	}
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;

	use insta::assert_debug_snapshot;
	use serde_json::from_str;

	use super::*;

	#[test]
	fn test_deserialize_channel() {
		let json = r#""2244305221""#;
		let chat: TelegramDestination = from_str(json).unwrap();
		assert_debug_snapshot!(chat, @r###"
ChannelExactUid(
    2244305221,
)
"###);

		let cases = [r#"2244305221"#, r#""-1002244305221""#, r#"-1002244305221"#];

		for case in cases {
			assert_eq!(from_str::<TelegramDestination>(case).unwrap(), chat);
		}
	}

	#[test]
	fn test_deserialize_group() {
		let json = r#""2244305221/7""#;
		let chat: TelegramDestination = from_str(json).unwrap();
		assert_debug_snapshot!(chat, @r###"
Group {
    id: 2244305221,
    thread_id: 7,
}
"###);
	}

	#[test]
	fn test_deserialize_errors() {
		// "invalid" is now parsed as a channel username
		let input = r#""invalid""#;
		let result: Result<TelegramDestination, _> = from_str(input);
		insta::assert_debug_snapshot!(result, @r###"
  Ok(
      ChannelUsername(
          "invalid",
      ),
  )
  "###);

		let input = r#"2244305221.5"#;
		let result: Result<TelegramDestination, _> = from_str(input);
		insta::assert_debug_snapshot!(result, @r###"
  Err(
      Error("data did not match any variant of untagged enum TelegramDestinationHelper", line: 0, column: 0),
  )
  "###);
	}

	#[test]
	fn test_deserialize_channels() {
		let toml_str = r#"
wtt = "2244305221"
journal = "-1002244305222"
alerts = "2244305223/7"
"#;

		let config_channels: BTreeMap<String, TelegramDestination> = toml::from_str(toml_str).unwrap();

		assert_debug_snapshot!(config_channels, @r###"
  {
      "alerts": Group {
          id: 2244305223,
          thread_id: 7,
      },
      "journal": ChannelExactUid(
          2244305222,
      ),
      "wtt": ChannelExactUid(
          2244305221,
      ),
  }
  "###);
	}

	#[test]
	fn test_deserialize_channel_username() {
		let json = r#""WatchingTT""#;
		let chat: TelegramDestination = from_str(json).unwrap();
		assert_debug_snapshot!(chat, @r###"
ChannelUsername(
    "WatchingTT",
)
"###);

		let json_with_at = r#""@WatchingTT""#;
		let chat_with_at: TelegramDestination = from_str(json_with_at).unwrap();
		assert_debug_snapshot!(chat_with_at, @r###"
ChannelUsername(
    "@WatchingTT",
)
"###);
	}

	#[test]
	fn test_destination_params_username() {
		let dest = TelegramDestination::ChannelUsername("WatchingTT".to_string());
		let params = dest.destination_params();
		assert_eq!(params, vec![("chat_id", "@WatchingTT".to_string())]);

		let dest_with_at = TelegramDestination::ChannelUsername("@WatchingTT".to_string());
		let params_with_at = dest_with_at.destination_params();
		assert_eq!(params_with_at, vec![("chat_id", "@WatchingTT".to_string())]);
	}

	#[test]
	fn test_deserialize_channels_with_username() {
		let toml_str = r#"
wtt = "2244305221"
watching = "WatchingTT"
alerts = "2244305223/7"
"#;

		let config_channels: BTreeMap<String, TelegramDestination> = toml::from_str(toml_str).unwrap();

		assert_debug_snapshot!(config_channels, @r###"
  {
      "alerts": Group {
          id: 2244305223,
          thread_id: 7,
      },
      "watching": ChannelUsername(
          "WatchingTT",
      ),
      "wtt": ChannelExactUid(
          2244305221,
      ),
  }
  "###);
	}
}
