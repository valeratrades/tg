//TODO!!!: This. Should be the entry point to all integration tests of rust projects, following https://matklad.github.io/2021/02/27/delete-cargo-integration-tests.html

#[cfg(test)]
mod tests {
	use tg::chat::TelegramDestination;

	#[test]
	fn test_channel_username_integration() {
		// Test that channel username "WatchingTT" works correctly
		let username = "WatchingTT";
		let dest = TelegramDestination::ChannelUsername(username.to_string());

		// Verify destination_params formats it correctly for Telegram API
		let params = dest.destination_params();
		assert_eq!(params.len(), 1);
		assert_eq!(params[0].0, "chat_id");
		assert_eq!(params[0].1, "@WatchingTT");

		// Test with @ prefix
		let dest_with_at = TelegramDestination::ChannelUsername("@WatchingTT".to_string());
		let params_with_at = dest_with_at.destination_params();
		assert_eq!(params_with_at[0].1, "@WatchingTT");
	}

	#[test]
	fn test_channel_username_parsing() {
		use std::str::FromStr;

		// Test parsing from string
		let dest = TelegramDestination::from_str("WatchingTT").unwrap();
		match dest {
			TelegramDestination::ChannelUsername(username) => {
				assert_eq!(username, "WatchingTT");
			}
			_ => panic!("Expected ChannelUsername variant"),
		}

		// Test with @ prefix
		let dest_with_at = TelegramDestination::from_str("@WatchingTT").unwrap();
		match dest_with_at {
			TelegramDestination::ChannelUsername(username) => {
				assert_eq!(username, "@WatchingTT");
			}
			_ => panic!("Expected ChannelUsername variant"),
		}
	}

	#[test]
	fn test_channel_username_in_config() {
		use std::collections::BTreeMap;

		// Test that channel usernames can be deserialized from config
		let toml_str = r#"
watching = "WatchingTT"
another = "@another_channel"
numeric = "2244305221"
"#;

		let channels: BTreeMap<String, TelegramDestination> = toml::from_str(toml_str).unwrap();

		// Verify WatchingTT was parsed as username
		match channels.get("watching").unwrap() {
			TelegramDestination::ChannelUsername(username) => {
				assert_eq!(username, "WatchingTT");
			}
			_ => panic!("Expected ChannelUsername for 'watching'"),
		}

		// Verify @another_channel was parsed as username
		match channels.get("another").unwrap() {
			TelegramDestination::ChannelUsername(username) => {
				assert_eq!(username, "@another_channel");
			}
			_ => panic!("Expected ChannelUsername for 'another'"),
		}

		// Verify numeric ID still works
		match channels.get("numeric").unwrap() {
			TelegramDestination::ChannelExactUid(id) => {
				assert_eq!(*id, 2244305221);
			}
			_ => panic!("Expected ChannelExactUid for 'numeric'"),
		}
	}

	#[test]
	fn test_serialization_deserialization_with_username() {
		// Test JSON serialization/deserialization
		let dest = TelegramDestination::ChannelUsername("WatchingTT".to_string());

		// Verify it can be serialized
		let json = serde_json::to_string(&dest).unwrap();
		assert!(json.contains("WatchingTT"));

		// Verify it can be deserialized back
		let deserialized: TelegramDestination = serde_json::from_str(&json).unwrap();
		match &deserialized {
			TelegramDestination::ChannelUsername(username) => {
				assert_eq!(username, "WatchingTT");
			}
			_ => panic!("Expected ChannelUsername after deserialization"),
		}

		assert_eq!(dest, deserialized);
	}
}
