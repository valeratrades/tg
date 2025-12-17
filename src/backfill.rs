use std::{
	collections::BTreeMap,
	io::{Read, Seek, SeekFrom, Write},
	path::PathBuf,
};

use chrono::{DateTime, Utc};
use eyre::{Result, eyre};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use v_utils::{trades::Timeframe, xdg_state_file};
use xattr::FileExt as _;

use crate::{chat_filepath, config::AppConfig, server::format_message_append};

const MAX_MESSAGES_BACKFILL: i64 = 1000;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SyncTimestamps {
	/// Maps channel name -> last synced update_id
	pub channels: BTreeMap<String, i64>,
}

impl SyncTimestamps {
	pub fn load() -> Self {
		let path = Self::file_path();
		if !path.exists() {
			return Self::default();
		}

		match std::fs::read_to_string(&path) {
			Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
			Err(e) => {
				warn!("Failed to read sync_timestamps.json: {}", e);
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

	pub fn file_path() -> PathBuf {
		xdg_state_file!("sync_timestamps.json")
	}
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
	update_id: i64,
	#[serde(default)]
	message: Option<TelegramMessage>,
	#[serde(default)]
	channel_post: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
	date: i64,
	chat: TelegramChat,
	#[serde(default)]
	text: Option<String>,
	#[serde(default)]
	message_thread_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
	id: i64,
}

#[derive(Debug, Deserialize)]
struct GetUpdatesResponse {
	ok: bool,
	#[serde(default)]
	result: Vec<TelegramUpdate>,
	#[serde(default)]
	description: Option<String>,
}

/// Run a single backfill operation for all configured channels
pub async fn backfill(config: &AppConfig, bot_token: &str) -> Result<()> {
	let mut sync_timestamps = SyncTimestamps::load();
	info!("Starting backfill, loaded sync timestamps: {:?}", sync_timestamps.channels.keys().collect::<Vec<_>>());

	// Find the minimum offset across all channels (or 0 if none)
	let min_offset = sync_timestamps.channels.values().copied().min().unwrap_or(0);

	// Fetch updates from Telegram
	let updates = fetch_updates(bot_token, min_offset, MAX_MESSAGES_BACKFILL).await?;
	info!("Fetched {} updates from Telegram", updates.len());

	if updates.is_empty() {
		return Ok(());
	}

	// Group updates by destination
	let mut updates_by_dest: BTreeMap<String, Vec<(TelegramMessage, i64)>> = BTreeMap::new();

	for update in updates {
		let msg = update.message.or(update.channel_post);
		if let Some(msg) = msg {
			// Try to match this message to a configured channel
			for (name, dest) in &config.channels {
				let matches = match dest {
					crate::config::TelegramDestination::ChannelExactUid(id) => msg.chat.id == -100 * (*id as i64) || msg.chat.id == *id as i64,
					crate::config::TelegramDestination::ChannelUsername(_) => {
						// Can't match usernames to chat IDs without additional API call
						false
					}
					crate::config::TelegramDestination::Group { id, thread_id } =>
						(msg.chat.id == -100 * (*id as i64) || msg.chat.id == *id as i64) && msg.message_thread_id == Some(*thread_id as i64),
				};

				if matches {
					updates_by_dest.entry(name.clone()).or_default().push((msg, update.update_id));
					break;
				}
			}
		}
	}

	// Process each destination
	for (dest_name, messages) in updates_by_dest {
		let last_synced = sync_timestamps.channels.get(&dest_name).copied().unwrap_or(0);

		// Filter to only new messages
		let new_messages: Vec<_> = messages.into_iter().filter(|(_, update_id)| *update_id > last_synced).collect();

		if new_messages.is_empty() {
			debug!("No new messages for {}", dest_name);
			continue;
		}

		info!("Processing {} new messages for {}", new_messages.len(), dest_name);

		// Merge messages into the local file
		merge_messages_to_file(&dest_name, &new_messages)?;

		// Update sync timestamp
		if let Some(max_update_id) = new_messages.iter().map(|(_, id)| *id).max() {
			sync_timestamps.channels.insert(dest_name, max_update_id);
		}
	}

	// Save updated timestamps
	sync_timestamps.save()?;
	info!("Backfill complete, saved sync timestamps");

	Ok(())
}

async fn fetch_updates(bot_token: &str, offset: i64, limit: i64) -> Result<Vec<TelegramUpdate>> {
	let url = format!("https://api.telegram.org/bot{}/getUpdates", bot_token);
	let client = reqwest::Client::new();

	let mut params = vec![("limit", limit.to_string()), ("timeout", "0".to_string())];

	if offset > 0 {
		params.push(("offset", offset.to_string()));
	}

	debug!("Fetching updates with offset={}, limit={}", offset, limit);
	let res = client.get(&url).query(&params).send().await?;

	let response: GetUpdatesResponse = res.json().await?;

	if !response.ok {
		return Err(eyre!("Telegram API error: {:?}", response.description));
	}

	Ok(response.result)
}

fn merge_messages_to_file(dest_name: &str, messages: &[(TelegramMessage, i64)]) -> Result<()> {
	let chat_filepath = chat_filepath(dest_name);

	// Read existing xattr for last write time
	let last_write_datetime: Option<DateTime<Utc>> = std::fs::File::open(&chat_filepath)
		.ok()
		.and_then(|file| file.get_xattr("user.last_changed").ok())
		.flatten()
		.and_then(|v| String::from_utf8(v).ok())
		.and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
		.map(|dt| dt.with_timezone(&Utc));

	let mut file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&chat_filepath)?;

	// Trim trailing whitespace
	let mut file_contents = String::new();
	file.read_to_string(&mut file_contents)?;
	let truncate_including_pos = file_contents.trim_end().len() + 1;
	file.set_len(truncate_including_pos as u64)?;
	file.seek(SeekFrom::End(0))?;

	// Sort messages by date
	let mut sorted_messages: Vec<_> = messages.iter().collect();
	sorted_messages.sort_by_key(|(msg, _)| msg.date);

	let mut last_write = last_write_datetime;

	for (msg, _) in sorted_messages {
		if let Some(text) = &msg.text {
			let msg_time = DateTime::from_timestamp(msg.date, 0).unwrap_or_else(Utc::now);
			let formatted = format_message_append(text, last_write, msg_time);
			file.write_all(formatted.as_bytes())?;
			last_write = Some(msg_time);
		}
	}

	// Update xattr
	if let Some(last) = last_write {
		file.set_xattr("user.last_changed", last.to_rfc3339().as_bytes())?;
	}

	Ok(())
}

/// Spawns a background task that runs backfill periodically
pub fn spawn_periodic_backfill(config: AppConfig, bot_token: String, interval: Timeframe) -> tokio::task::JoinHandle<()> {
	let interval_duration = interval.duration();
	info!("Starting periodic backfill with interval: {}", interval);

	tokio::spawn(async move {
		let mut interval_timer = tokio::time::interval(interval_duration);

		loop {
			interval_timer.tick().await;

			match backfill(&config, &bot_token).await {
				Ok(()) => debug!("Periodic backfill completed successfully"),
				Err(e) => warn!("Periodic backfill failed: {}", e),
			}
		}
	})
}
