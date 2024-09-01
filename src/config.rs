use std::collections::BTreeMap; // so snapshot tests work
use std::{fmt, path::Path, str::FromStr};

use eyre::{eyre, Result};
use serde::{de, Deserialize, Deserializer, Serialize};
use v_utils::macros::MyConfigPrimitives;

#[derive(Debug, Default, derive_new::new, Clone, MyConfigPrimitives)]
pub struct AppConfig {
	pub channels: BTreeMap<String, TelegramDestination>,
	pub localhost_port: u16,
}

#[derive(Clone, Debug, derive_new::new, Copy, PartialEq, Eq, Serialize, Hash)]
/// Doesn't store "-100" prefix
pub enum TelegramDestination {
	Channel(u64),
	Group { id: u64, thread_id: u64 },
}
impl TelegramDestination {
	pub fn destination_params(&self) -> Vec<(&str, String)> {
		match self {
			Self::Channel(chat_id) => vec![("chat_id", format!("-100{chat_id}"))],
			Self::Group { id, thread_id } => vec![("chat_id", format!("-100{id}")), ("message_thread_id", thread_id.to_string())],
		}
	}

	pub fn display(&self, config: &AppConfig) -> String {
		match config.channels.iter().find(|(_, &td)| td == *self) {
			Some((key, _)) => key.clone(),
			None => match self {
				Self::Channel(id) => format!("{}", id),
				Self::Group { id, thread_id } => format!("{}_slash_{}", id, thread_id),
			},
		}
	}
}

impl Default for TelegramDestination {
	fn default() -> Self {
		Self::Channel(0)
	}
}

impl<'de> Deserialize<'de> for TelegramDestination {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>, {
		struct TelegramChatVisitor;

		impl<'de> de::Visitor<'de> for TelegramChatVisitor {
			type Value = TelegramDestination;

			fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
				formatter.write_str("a string or number representing a channel or group")
			}

			fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
			where
				E: de::Error, {
				parse_telegram_destination_str(value).map_err(de::Error::custom)
			}

			fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
			where
				E: de::Error, {
				Ok(TelegramDestination::Channel(value))
			}

			fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
			where
				E: de::Error, {
				parse_telegram_destination_str(&value.to_string()).map_err(de::Error::custom)
			}
		}

		deserializer.deserialize_any(TelegramChatVisitor)
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
	if let Some((id_str, thread_id_str)) = s.split_once('/') {
		let id = parse_chat_id(id_str)?;
		let thread_id = u64::from_str(thread_id_str).map_err(|e| eyre!("Failed to parse thread ID: {}", e))?;
		Ok(TelegramDestination::Group { id, thread_id })
	} else {
		let id = parse_chat_id(s)?;
		Ok(TelegramDestination::Channel(id))
	}
}

impl AppConfig {
	pub fn read(path: &Path) -> Result<Self, config::ConfigError> {
		let builder = config::Config::builder().add_source(config::File::with_name(&format!("{}", path.display())));

		let settings: config::Config = builder.build()?;
		let settings: Self = settings.try_deserialize()?;

		assert_eq!(
			format!("{}", crate::server::VAR_DIR.display()),
			"/var/local/tg",
			"VAR_DIR must be /var/local/tg (because next line)"
		);
		let output = std::process::Command::new("sudo")
			.arg("chmod")
			.arg("a+w")
			.arg("/var/local")
			.output()
			.map_err(|e| config::ConfigError::Foreign(Box::new(e)))?;

		if !output.status.success() {
			let error_message = String::from_utf8_lossy(&output.stderr);
			return Err(config::ConfigError::Foreign(Box::new(std::io::Error::new(
				std::io::ErrorKind::Other,
				format!("Failed to set permissions on /var/local: {}", error_message),
			))));
		}
		std::fs::create_dir_all(*crate::server::VAR_DIR).map_err(|e| config::ConfigError::Foreign(Box::new(e)))?;

		Ok(settings)
	}
}

#[cfg(test)]
mod tests {
	use insta::assert_debug_snapshot;
	use serde_json::from_str;

	use super::*;

	#[test]
	fn test_deserialize_channel() {
		let json = r#""2244305221""#;
		let chat: TelegramDestination = from_str(json).unwrap();
		assert_debug_snapshot!(chat, @r###"
Channel(
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
		let cases = [
			(r#""invalid""#, "invalid digit found in string"),
			(
				r#"2244305221.5"#,
				"invalid type: floating point `2244305221.5`, expected a string or number representing a channel or group",
			),
		];

		for (json, expected_error) in cases {
			let result: Result<TelegramDestination, _> = from_str(json);
			assert!(result.is_err());
			assert!(result.unwrap_err().to_string().contains(expected_error));
		}
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
      "journal": Channel(
          2244305222,
      ),
      "wtt": Channel(
          2244305221,
      ),
  }
  "###);
	}
}
