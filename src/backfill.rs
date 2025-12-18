use std::{
	collections::BTreeMap,
	io::{Read, Seek, SeekFrom, Write},
	path::PathBuf,
};

use chrono::{DateTime, Utc};
use eyre::{Result, eyre};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use v_utils::xdg_state_file;
use xattr::FileExt as _;

use crate::{
	chat_filepath,
	config::{AppConfig, TelegramDestination},
	server::format_message_append,
};

const MAX_MESSAGES_PER_FETCH: i64 = 1000;

/// Calculate the full chat ID that Telegram uses (with -100 prefix for channels/supergroups)
fn telegram_chat_id(id: u64) -> i64 {
	format!("-100{}", id).parse().unwrap()
}

/// Check if a message matches a configured destination
fn message_matches_destination(msg: &TelegramMessage, dest: &TelegramDestination) -> bool {
	match dest {
		TelegramDestination::ChannelExactUid(id) => {
			let full_id = telegram_chat_id(*id);
			msg.chat.id == full_id || msg.chat.id == *id as i64
		}
		TelegramDestination::ChannelUsername(_) => {
			// Can't match usernames to chat IDs without additional API call
			false
		}
		TelegramDestination::Group { id, thread_id } => {
			let full_id = telegram_chat_id(*id);
			(msg.chat.id == full_id || msg.chat.id == *id as i64) && msg.message_thread_id == Some(*thread_id as i64)
		}
	}
}

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

#[derive(Clone, Debug, Deserialize)]
struct TelegramMessage {
	date: i64,
	chat: TelegramChat,
	#[serde(default)]
	text: Option<String>,
	#[serde(default)]
	message_thread_id: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
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
	// Add 1 because Telegram's offset returns updates with update_id >= offset,
	// and we've already processed up to the stored offset
	let min_offset = sync_timestamps.channels.values().copied().min().unwrap_or(-1) + 1;

	// Fetch updates from Telegram with pagination
	let mut all_updates = Vec::new();
	let mut current_offset = min_offset;
	loop {
		let updates = fetch_updates(bot_token, current_offset, MAX_MESSAGES_PER_FETCH).await?;
		let batch_size = updates.len();
		info!("Fetched {} updates from Telegram (offset={})", batch_size, current_offset);

		if updates.is_empty() {
			break;
		}

		// Update offset for next iteration
		if let Some(last_update) = updates.last() {
			current_offset = last_update.update_id + 1;
		}

		all_updates.extend(updates);

		// If we got fewer than the limit, there are no more updates
		if batch_size < MAX_MESSAGES_PER_FETCH as usize {
			break;
		}
	}

	if all_updates.is_empty() {
		return Ok(());
	}

	info!("Total updates fetched: {}", all_updates.len());

	// Group updates by destination
	let mut updates_by_dest: BTreeMap<String, Vec<(TelegramMessage, i64)>> = BTreeMap::new();

	for update in all_updates {
		let msg = update.message.or(update.channel_post);
		if let Some(msg) = msg {
			// Try to match this message to a configured channel
			let mut matched = false;
			for (name, dest) in &config.channels {
				if message_matches_destination(&msg, dest) {
					debug!("Message from chat {} matched channel '{}' (dest: {:?})", msg.chat.id, name, dest);
					updates_by_dest.entry(name.clone()).or_default().push((msg.clone(), update.update_id));
					matched = true;
					break;
				}
			}

			if !matched {
				debug!("Unmatched message from chat_id={}, thread_id={:?}", msg.chat.id, msg.message_thread_id);
			}
		}
	}

	// Process each destination
	for (dest_name, messages) in updates_by_dest {
		let last_synced = sync_timestamps.channels.get(&dest_name).copied().unwrap_or(0);

		// Filter to only new messages
		let mut new_messages: Vec<_> = messages.into_iter().filter(|(_, update_id)| *update_id > last_synced).collect();

		if new_messages.is_empty() {
			debug!("No new messages for {}", dest_name);
			continue;
		}

		// Limit to most recent max_messages_per_chat messages per chat
		let max_per_chat = config.max_messages_per_chat();
		if new_messages.len() > max_per_chat {
			// Sort by update_id and keep the most recent
			new_messages.sort_by_key(|(_, update_id)| *update_id);
			let skip = new_messages.len() - max_per_chat;
			new_messages = new_messages.into_iter().skip(skip).collect();
			info!("Limiting {} to {} most recent messages (skipped {})", dest_name, max_per_chat, skip);
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

	// Trim trailing whitespace, keeping at most one newline
	let mut file_contents = String::new();
	file.read_to_string(&mut file_contents)?;
	let trimmed_len = file_contents.trim_end().len();
	// Cap at file length to avoid extending the file if it doesn't end with whitespace
	let truncate_to = std::cmp::min(trimmed_len + 1, file_contents.len());
	file.set_len(truncate_to as u64)?;
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
