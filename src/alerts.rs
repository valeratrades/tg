//! Alerts channel monitoring - shows desktop notifications for unread messages.
//!
//! Persists notification state to avoid re-notifying for the same messages.
//! Notifications never expire: they close once the message is read elsewhere, and a user dismissal marks it read.

use std::{collections::HashMap, path::PathBuf};

use eyre::Result;
use grammers_client::Client;
use grammers_tl_types as tl;
use serde::{Deserialize, Serialize};
use tg::telegram_chat_id;
use tracing::{debug, info, warn};
use zbus::zvariant::Value;

use crate::{config::LiveSettings, mtproto};

#[zbus::proxy(
	interface = "org.freedesktop.Notifications",
	default_service = "org.freedesktop.Notifications",
	default_path = "/org/freedesktop/Notifications"
)]
pub trait Notifications {
	#[allow(clippy::too_many_arguments)] // fixed by the freedesktop spec
	fn notify(
		&self,
		app_name: &str,
		replaces_id: u32,
		app_icon: &str,
		summary: &str,
		body: &str,
		actions: &[&str],
		hints: HashMap<&str, Value<'_>>,
		expire_timeout: i32,
	) -> zbus::Result<u32>;
	fn close_notification(&self, id: u32) -> zbus::Result<()>;
	#[zbus(signal)]
	fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// `NotificationClosed` reason per the freedesktop spec; expiry is 1, `CloseNotification` is 3.
const DISMISSED_BY_USER: u32 = 2;

/// Persisted state for alerts channel notifications
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AlertsState {
	/// message_id -> notification id
	notified: HashMap<i32, u32>,
}

impl AlertsState {
	fn file_path() -> PathBuf {
		v_utils::xdg_state_file!("alerts_channel.json")
	}

	fn load() -> Self {
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

	fn save(&self) -> Result<()> {
		let path = Self::file_path();
		let content = serde_json::to_string_pretty(self)?;
		std::fs::write(&path, content)?;
		Ok(())
	}

	/// Clear the state file (called at session start)
	pub fn clear() -> Result<()> {
		let path = Self::file_path();
		if path.exists() {
			let state = Self::load();
			if !state.notified.is_empty() {
				let notifications = NotificationsProxyBlocking::new(&zbus::blocking::Connection::session()?)?;
				for id in state.notified.values() {
					notifications.close_notification(*id)?;
				}
			}
			std::fs::remove_file(&path)?;
		}
		Ok(())
	}
}

/// `(group_id, topic_id)` of the configured alerts channel; `topic_id` is `None` for a plain channel or the General topic
fn alerts_peer(settings: &LiveSettings) -> Result<Option<(u64, Option<u64>)>> {
	let cfg = settings.config()?;
	let Some(alerts_channel) = &cfg.alerts_channel else {
		return Ok(None);
	};
	match alerts_channel.as_group_topic() {
		Some((g, t)) => Ok(Some((g, if t == 1 { None } else { Some(t) }))),
		None => {
			warn!("alerts_channel must be a numeric ID, not a username");
			Ok(None)
		}
	}
}

/// Check alerts channel and show/dismiss notifications as needed
pub async fn check_alerts(client: &Client, settings: &LiveSettings, notifications: &NotificationsProxy<'_>) -> Result<()> {
	let Some((group_id, topic_id)) = alerts_peer(settings)? else {
		return Ok(());
	};

	debug!("Checking alerts channel: group_id={group_id}, topic_id={topic_id:?}");

	let unread = fetch_unread_message_ids(client, group_id, topic_id).await?;
	let unread_ids: std::collections::HashSet<i32> = unread.iter().map(|(id, _)| *id).collect();

	let mut state = AlertsState::load();

	let to_remove: Vec<i32> = state.notified.keys().filter(|id| !unread_ids.contains(id)).copied().collect();

	for msg_id in to_remove {
		if let Some(id) = state.notified.remove(&msg_id) {
			match notifications.close_notification(id).await {
				Ok(()) => info!("Closed notification for msg:{msg_id} (id {id})"),
				Err(e) => warn!("Failed to close notification for msg:{msg_id} (id {id}): {e}"),
			}
		}
	}

	for (msg_id, text) in &unread {
		if !state.notified.contains_key(msg_id) {
			let hints = HashMap::from([("urgency", Value::U8(2))]); // critical
			match notifications.notify("tg", 0, "", "Telegram Alert", text, &[], hints, 0).await {
				Ok(id) => {
					info!("Spawned notification for msg:{msg_id} (id {id})");
					state.notified.insert(*msg_id, id);
				}
				Err(e) => {
					warn!("Failed to spawn notification for msg:{msg_id}: {e}");
				}
			}
		}
	}

	state.save()?;

	Ok(())
}
/// Mark the alert as read in telegram if the user dismissed its notification; expiry and our own closes are ignored.
pub async fn on_notification_closed(client: &Client, settings: &LiveSettings, id: u32, reason: u32) -> Result<()> {
	if reason != DISMISSED_BY_USER {
		return Ok(());
	}
	let mut state = AlertsState::load();
	let Some(msg_id) = state.notified.iter().find_map(|(m, n)| (*n == id).then_some(*m)) else {
		return Ok(()); // not one of ours
	};
	state.notified.remove(&msg_id);
	state.save()?;

	let (group_id, topic_id) = alerts_peer(settings)?.expect("notifications are only spawned for a configured alerts channel");
	let input_peer = mtproto::get_input_peer(client, telegram_chat_id(group_id)).await?;
	match topic_id {
		Some(topic_id) => {
			client
				.invoke(&tl::functions::messages::ReadDiscussion {
					peer: input_peer,
					msg_id: topic_id as i32,
					read_max_id: msg_id,
				})
				.await?;
		}
		None => {
			let tl::enums::InputPeer::Channel(c) = input_peer else {
				eyre::bail!("alerts channel {group_id} resolved to a non-channel peer");
			};
			let channel = tl::enums::InputChannel::Channel(tl::types::InputChannel {
				channel_id: c.channel_id,
				access_hash: c.access_hash,
			});
			client.invoke(&tl::functions::channels::ReadHistory { channel, max_id: msg_id }).await?;
		}
	}
	info!("Marked msg:{msg_id} read after its notification was dismissed");
	Ok(())
}
/// Fetch unread message IDs from the alerts channel
async fn fetch_unread_message_ids(client: &Client, group_id: u64, topic_id: Option<u64>) -> Result<Vec<(i32, String)>> {
	let chat_id = telegram_chat_id(group_id);

	// Get InputPeer by iterating dialogs
	let input_peer = mtproto::get_input_peer(client, chat_id).await?;

	// Get unread count and read inbox max ID from the dialog
	let mut dialogs = client.iter_dialogs();
	let expected_id = mtproto::extract_channel_id(chat_id);

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
			if let tl::enums::Message::Message(m) = msg
				&& m.id > read_inbox_max_id
				&& !m.out
			{
				// Unread incoming message
				let text = if m.message.len() > 100 { format!("{}...", &m.message[..100]) } else { m.message.clone() };
				unread_messages.push((m.id, text));
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
			if let tl::enums::Message::Message(m) = msg
				&& m.id > read_inbox_max_id
				&& !m.out
			{
				let text = if m.message.len() > 100 { format!("{}...", &m.message[..100]) } else { m.message.clone() };
				unread_messages.push((m.id, text));
			}
		}
	}

	Ok(unread_messages)
}
