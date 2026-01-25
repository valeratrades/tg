//! Alerts channel monitoring - shows desktop notifications for unread messages.
//!
//! Persists notification state to avoid re-notifying for the same messages.
//! Spawns `notify-send` with no timeout for unread alerts, kills them when read.

use std::collections::HashMap;

use eyre::Result;
use grammers_client::Client;
use grammers_tl_types as tl;
use serde::{Deserialize, Serialize};
use tg::telegram_chat_id;
use tracing::{debug, info, warn};

use crate::config::LiveSettings;

/// Persisted state for alerts channel notifications
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AlertsState {
	/// Maps message_id -> PID of the notify-send process showing it
	pub notified_pids: HashMap<i32, u32>,
}

impl AlertsState {
	fn file_path() -> std::path::PathBuf {
		v_utils::xdg_state_file!("alerts_channel.json")
	}

	pub fn load() -> Self {
		let path = Self::file_path();
		if !path.exists() {
			return Self::default();
		}

		match std::fs::read_to_string(&path) {
			Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
			Err(e) => {
				warn!("Failed to read alerts_channel.json: {e}");
				Self::default()
			}
		}
	}

	pub fn save(&self) -> Result<()> {
		let path = Self::file_path();
		let content = serde_json::to_string_pretty(self)?;
		std::fs::write(&path, content)?;
		Ok(())
	}

	/// Clear the state file (called at session start)
	pub fn clear() -> Result<()> {
		let path = Self::file_path();
		if path.exists() {
			// Kill any existing notification processes before clearing
			let state = Self::load();
			for (msg_id, pid) in &state.notified_pids {
				kill_notification(*pid, *msg_id);
			}
			std::fs::remove_file(&path)?;
		}
		Ok(())
	}
}

/// Check alerts channel and show/dismiss notifications as needed
pub async fn check_alerts(client: &Client, settings: &LiveSettings) -> Result<()> {
	let cfg = settings.config()?;
	let alerts_channel = match &cfg.alerts_channel {
		Some(c) => c,
		None => return Ok(()), // No alerts channel configured
	};

	let (group_id, topic_id) = match alerts_channel.as_group_topic() {
		Some((g, t)) => (g, if t == 1 { None } else { Some(t) }),
		None => {
			warn!("alerts_channel must be a numeric ID, not a username");
			return Ok(());
		}
	};

	debug!("Checking alerts channel: group_id={group_id}, topic_id={topic_id:?}");

	// Fetch unread messages
	let unread = fetch_unread_message_ids(client, group_id, topic_id).await?;
	let unread_ids: std::collections::HashSet<i32> = unread.iter().map(|(id, _)| *id).collect();

	// Load current state
	let mut state = AlertsState::load();

	// Kill notifications for messages that are now read
	let to_remove: Vec<i32> = state.notified_pids.keys().filter(|id| !unread_ids.contains(id)).copied().collect();

	for msg_id in to_remove {
		if let Some(pid) = state.notified_pids.remove(&msg_id) {
			kill_notification(pid, msg_id);
		}
	}

	// Spawn notifications for new unread messages
	for (msg_id, text) in &unread {
		if !state.notified_pids.contains_key(msg_id) {
			match spawn_notification("Telegram Alert", text) {
				Ok(pid) => {
					info!("Spawned notification for msg:{msg_id} (pid {pid})");
					state.notified_pids.insert(*msg_id, pid);
				}
				Err(e) => {
					warn!("Failed to spawn notification for msg:{msg_id}: {e}");
				}
			}
		}
	}

	// Save updated state
	state.save()?;

	Ok(())
}
/// Kill a notification process by PID
fn kill_notification(pid: u32, msg_id: i32) {
	match std::process::Command::new("kill").arg(pid.to_string()).output() {
		Ok(output) => {
			if output.status.success() {
				info!("Killed notification for msg:{msg_id} (pid {pid})");
			} else {
				// Process already dead - this is fine
				info!("Notification for msg:{msg_id} (pid {pid}) already dismissed");
			}
		}
		Err(e) => {
			info!("Could not kill notification pid {pid}: {e}");
		}
	}
}
/// Spawn a desktop notification that stays until dismissed
fn spawn_notification(title: &str, body: &str) -> Result<u32> {
	let child = std::process::Command::new("notify-send")
		.arg("--urgency=critical")
		.arg("--expire-time=0") // 0 = never expire
		.arg(title)
		.arg(body)
		.spawn()?;

	Ok(child.id())
}
/// Fetch unread message IDs from the alerts channel
async fn fetch_unread_message_ids(client: &Client, group_id: u64, topic_id: Option<u64>) -> Result<Vec<(i32, String)>> {
	let chat_id = telegram_chat_id(group_id);

	// Get InputPeer by iterating dialogs
	let input_peer = get_input_peer(client, chat_id).await?;

	// Get unread count and read inbox max ID from the dialog
	let mut dialogs = client.iter_dialogs();
	let expected_id = extract_channel_id(chat_id);

	let mut read_inbox_max_id = 0;

	while let Some(dialog) = dialogs.next().await? {
		if let tl::enums::Dialog::Dialog(d) = &dialog.raw {
			let peer_id = match &d.peer {
				tl::enums::Peer::Channel(c) => c.channel_id,
				tl::enums::Peer::Chat(c) => c.chat_id,
				tl::enums::Peer::User(u) => u.user_id,
			};

			if peer_id == expected_id {
				read_inbox_max_id = d.read_inbox_max_id;
				debug!("Found dialog: unread_count={}, read_inbox_max_id={}", d.unread_count, d.read_inbox_max_id);
				break;
			}
		}
	}

	// Fetch recent messages and filter to unread ones
	let mut unread_messages = Vec::new();

	if let Some(topic_id) = topic_id {
		// Forum topic - use GetReplies
		let request = tl::functions::messages::GetReplies {
			peer: input_peer,
			msg_id: topic_id as i32,
			offset_id: 0,
			offset_date: 0,
			add_offset: 0,
			limit: 50, // Check last 50 messages
			max_id: 0,
			min_id: read_inbox_max_id, // Only get messages after the last read
			hash: 0,
		};

		let result = client.invoke(&request).await?;
		let messages = match result {
			tl::enums::messages::Messages::Messages(m) => m.messages,
			tl::enums::messages::Messages::Slice(m) => m.messages,
			tl::enums::messages::Messages::ChannelMessages(m) => m.messages,
			tl::enums::messages::Messages::NotModified(_) => vec![],
		};

		for msg in messages {
			if let tl::enums::Message::Message(m) = msg {
				if m.id > read_inbox_max_id && !m.out {
					// Unread incoming message
					let text = if m.message.len() > 100 { format!("{}...", &m.message[..100]) } else { m.message.clone() };
					unread_messages.push((m.id, text));
				}
			}
		}
	} else {
		// Regular channel - use GetHistory
		let request = tl::functions::messages::GetHistory {
			peer: input_peer,
			offset_id: 0,
			offset_date: 0,
			add_offset: 0,
			limit: 50,
			max_id: 0,
			min_id: read_inbox_max_id,
			hash: 0,
		};

		let result = client.invoke(&request).await?;
		let messages = match result {
			tl::enums::messages::Messages::Messages(m) => m.messages,
			tl::enums::messages::Messages::Slice(m) => m.messages,
			tl::enums::messages::Messages::ChannelMessages(m) => m.messages,
			tl::enums::messages::Messages::NotModified(_) => vec![],
		};

		for msg in messages {
			if let tl::enums::Message::Message(m) = msg {
				if m.id > read_inbox_max_id && !m.out {
					let text = if m.message.len() > 100 { format!("{}...", &m.message[..100]) } else { m.message.clone() };
					unread_messages.push((m.id, text));
				}
			}
		}
	}

	Ok(unread_messages)
}
/// Extract channel ID from chat_id (strips -100 prefix)
fn extract_channel_id(chat_id: i64) -> i64 {
	let s = chat_id.to_string();
	if let Some(stripped) = s.strip_prefix("-100") {
		stripped.parse().unwrap_or(0)
	} else {
		chat_id.abs()
	}
}
/// Get InputPeer from chat_id by iterating dialogs
async fn get_input_peer(client: &Client, chat_id: i64) -> Result<tl::enums::InputPeer> {
	let mut dialogs = client.iter_dialogs();
	let expected_id = extract_channel_id(chat_id);

	while let Some(dialog) = dialogs.next().await? {
		if let tl::enums::Dialog::Dialog(d) = &dialog.raw {
			let peer_id = match &d.peer {
				tl::enums::Peer::Channel(c) => c.channel_id,
				tl::enums::Peer::Chat(c) => c.chat_id,
				tl::enums::Peer::User(u) => u.user_id,
			};

			if peer_id == expected_id {
				let peer = dialog.peer();
				match peer {
					grammers_client::types::Peer::Group(g) =>
						if let tl::enums::Chat::Channel(ch) = &g.raw {
							return Ok(tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
								channel_id: ch.id,
								access_hash: ch.access_hash.unwrap_or(0),
							}));
						},
					grammers_client::types::Peer::Channel(c) => {
						return Ok(tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
							channel_id: c.raw.id,
							access_hash: c.raw.access_hash.unwrap_or(0),
						}));
					}
					_ => {}
				}
			}
		}
	}

	eyre::bail!("Could not find channel with id {chat_id} in dialogs")
}
