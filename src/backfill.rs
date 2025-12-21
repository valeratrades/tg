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
	config::{AppConfig, TopicsMetadata, extract_group_id},
	server::format_message_append,
};

const MAX_MESSAGES_PER_FETCH: i64 = 1000;

/// Sanitize topic name for use as filename: lowercase, replace spaces with underscores
fn sanitize_topic_name(name: &str) -> String {
	name.to_lowercase()
		.chars()
		.map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
		.collect::<String>()
		.trim_matches('_')
		.to_string()
}

/// Download an image from Telegram and save it locally, returning the relative filename
async fn download_telegram_image(bot_token: &str, file_id: &str, dest_name: &str, timestamp: i64) -> Result<String> {
	let client = reqwest::Client::new();

	// Get file path from Telegram
	let url = format!("https://api.telegram.org/bot{}/getFile", bot_token);
	let res = client.get(&url).query(&[("file_id", file_id)]).send().await?;
	let file_response: GetFileResponse = res.json().await?;

	let file_path = file_response.result.and_then(|r| r.file_path).ok_or_else(|| eyre!("No file_path in getFile response"))?;

	// Download the file
	let download_url = format!("https://api.telegram.org/file/bot{}/{}", bot_token, file_path);
	let image_bytes = client.get(&download_url).send().await?.bytes().await?;

	// Determine extension from file_path
	let ext = std::path::Path::new(&file_path).extension().and_then(|e| e.to_str()).unwrap_or("jpg");

	// Save to images subdirectory
	let images_dir = crate::server::DATA_DIR.get().unwrap().join("images");
	std::fs::create_dir_all(&images_dir)?;

	let filename = format!("{}_{}.{}", dest_name, timestamp, ext);
	let full_path = images_dir.join(&filename);
	std::fs::write(&full_path, &image_bytes)?;

	debug!("Downloaded image to {}", full_path.display());
	Ok(format!("images/{}", filename))
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SyncTimestamps {
	/// Maps "group_id/topic_id" -> last synced update_id
	pub topics: BTreeMap<String, i64>,
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
	caption: Option<String>,
	#[serde(default)]
	photo: Option<Vec<PhotoSize>>,
	#[serde(default)]
	message_thread_id: Option<i64>,
	#[serde(default)]
	forum_topic_created: Option<ForumTopicCreated>,
	#[serde(default)]
	forum_topic_edited: Option<ForumTopicEdited>,
}

#[derive(Clone, Debug, Deserialize)]
struct ForumTopicCreated {
	name: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ForumTopicEdited {
	#[serde(default)]
	name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct TelegramChat {
	id: i64,
}

#[derive(Clone, Debug, Deserialize)]
struct PhotoSize {
	file_id: String,
	width: u32,
	height: u32,
}

#[derive(Debug, Deserialize)]
struct GetFileResponse {
	result: Option<TelegramFile>,
}

#[derive(Debug, Deserialize)]
struct TelegramFile {
	file_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetUpdatesResponse {
	ok: bool,
	#[serde(default)]
	result: Vec<TelegramUpdate>,
	#[serde(default)]
	description: Option<String>,
}

/// A discovered topic destination
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TopicDestination {
	group_id: u64,
	topic_id: u64,
}

impl TopicDestination {
	fn key(&self) -> String {
		format!("{}/{}", self.group_id, self.topic_id)
	}
}

/// Run a single backfill operation for all configured forum groups
pub async fn backfill(config: &AppConfig, bot_token: &str) -> Result<()> {
	let mut sync_timestamps = SyncTimestamps::load();
	let mut topics_metadata = TopicsMetadata::load();
	info!("Starting backfill, loaded sync timestamps for {} topics", sync_timestamps.topics.len());

	// Find the minimum offset across all topics (or 0 if none)
	let min_offset = sync_timestamps.topics.values().copied().min().unwrap_or(-1) + 1;

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

	// Group updates by topic destination
	let mut updates_by_topic: BTreeMap<String, Vec<(TelegramMessage, i64)>> = BTreeMap::new();

	for update in all_updates {
		let msg = update.message.or(update.channel_post);
		if let Some(msg) = msg {
			// Extract group_id from chat.id
			let group_id = match extract_group_id(msg.chat.id) {
				Some(id) => id,
				None => {
					debug!("Could not extract group_id from chat_id={}", msg.chat.id);
					continue;
				}
			};

			// Check if this group is in our configured forum_groups
			if !config.forum_groups().contains(&group_id) {
				debug!("Message from unconfigured group {}, skipping", group_id);
				continue;
			}

			// Get topic_id (default to 1 for General topic if not specified)
			let topic_id = msg.message_thread_id.unwrap_or(1) as u64;

			let dest = TopicDestination { group_id, topic_id };
			let key = dest.key();

			// Extract topic name from service messages if available
			if let Some(created) = &msg.forum_topic_created {
				let name = sanitize_topic_name(&created.name);
				topics_metadata.set_topic_name(group_id, topic_id, name);
			} else if let Some(edited) = &msg.forum_topic_edited {
				if let Some(name) = &edited.name {
					let name = sanitize_topic_name(name);
					topics_metadata.set_topic_name(group_id, topic_id, name);
				}
			} else {
				// Only create default name if topic doesn't exist yet
				topics_metadata.ensure_topic(group_id, topic_id);
			}

			debug!("Message from group {} topic {} matched", group_id, topic_id);
			updates_by_topic.entry(key).or_default().push((msg.clone(), update.update_id));
		}
	}

	// Save updated topics metadata
	topics_metadata.save()?;

	// Process each topic
	for (topic_key, messages) in updates_by_topic {
		let last_synced = sync_timestamps.topics.get(&topic_key).copied().unwrap_or(0);

		// Filter to only new messages
		let mut new_messages: Vec<_> = messages.into_iter().filter(|(_, update_id)| *update_id > last_synced).collect();

		if new_messages.is_empty() {
			debug!("No new messages for {}", topic_key);
			continue;
		}

		// Limit to most recent max_messages_per_chat messages per topic
		let max_per_chat = config.max_messages_per_chat();
		if new_messages.len() > max_per_chat {
			// Sort by update_id and keep the most recent
			new_messages.sort_by_key(|(_, update_id)| *update_id);
			let skip = new_messages.len() - max_per_chat;
			new_messages = new_messages.into_iter().skip(skip).collect();
			info!("Limiting {} to {} most recent messages (skipped {})", topic_key, max_per_chat, skip);
		}

		info!("Processing {} new messages for {}", new_messages.len(), topic_key);

		// Parse topic_key to get group_id and topic_id
		let parts: Vec<&str> = topic_key.split('/').collect();
		let group_id: u64 = parts[0].parse().unwrap();
		let topic_id: u64 = parts[1].parse().unwrap();

		// Merge messages into the local file
		merge_messages_to_file(group_id, topic_id, &new_messages, bot_token, &topics_metadata).await?;

		// Update sync timestamp
		if let Some(max_update_id) = new_messages.iter().map(|(_, id)| *id).max() {
			sync_timestamps.topics.insert(topic_key, max_update_id);
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

/// Get the file path for a topic
pub fn topic_filepath(group_id: u64, topic_id: u64, metadata: &TopicsMetadata) -> std::path::PathBuf {
	let data_dir = crate::server::DATA_DIR.get().unwrap();
	let group_name = metadata.group_name(group_id);
	let topic_name = metadata.topic_name(group_id, topic_id);

	let group_dir = data_dir.join(&group_name);
	group_dir.join(format!("{}.md", topic_name))
}

/// Ensure the parent directory for a topic file exists
pub fn ensure_topic_dir(group_id: u64, metadata: &TopicsMetadata) -> Result<()> {
	let data_dir = crate::server::DATA_DIR.get().unwrap();
	let group_name = metadata.group_name(group_id);
	let group_dir = data_dir.join(&group_name);
	std::fs::create_dir_all(&group_dir)?;
	Ok(())
}

async fn merge_messages_to_file(group_id: u64, topic_id: u64, messages: &[(TelegramMessage, i64)], bot_token: &str, metadata: &TopicsMetadata) -> Result<()> {
	ensure_topic_dir(group_id, metadata)?;
	let chat_filepath = topic_filepath(group_id, topic_id, metadata);
	let dest_name = format!("{}_{}", group_id, topic_id);

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
		let msg_time = DateTime::from_timestamp(msg.date, 0).unwrap_or_else(Utc::now);

		// Handle photo messages
		if let Some(photos) = &msg.photo {
			// Get the largest photo (last in array)
			if let Some(largest) = photos.iter().max_by_key(|p| p.width * p.height) {
				match download_telegram_image(bot_token, &largest.file_id, &dest_name, msg.date).await {
					Ok(image_path) => {
						// Use caption if available, otherwise just the image
						let content = match &msg.caption {
							Some(caption) => format!("![]({})\n{}", image_path, caption),
							None => format!("![]({})", image_path),
						};
						let formatted = format_message_append(&content, last_write, msg_time);
						file.write_all(formatted.as_bytes())?;
						last_write = Some(msg_time);
					}
					Err(e) => {
						warn!("Failed to download image: {}", e);
						// Still include caption if present
						if let Some(caption) = &msg.caption {
							let formatted = format_message_append(&format!("[image failed to download]\n{}", caption), last_write, msg_time);
							file.write_all(formatted.as_bytes())?;
							last_write = Some(msg_time);
						}
					}
				}
			}
		} else if let Some(text) = &msg.text {
			// Regular text message
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_sanitize_topic_name() {
		// Basic lowercase
		assert_eq!(sanitize_topic_name("Journal"), "journal");
		assert_eq!(sanitize_topic_name("ALERTS"), "alerts");

		// Spaces become underscores
		assert_eq!(sanitize_topic_name("My Topic"), "my_topic");
		assert_eq!(sanitize_topic_name("Work Notes"), "work_notes");

		// Special characters become underscores
		assert_eq!(sanitize_topic_name("alerts!"), "alerts");
		assert_eq!(sanitize_topic_name("topic/name"), "topic_name");
		assert_eq!(sanitize_topic_name("topic:name"), "topic_name");

		// Emojis are stripped
		assert_eq!(sanitize_topic_name("🔥 Hot Takes"), "hot_takes");
		assert_eq!(sanitize_topic_name("📝 Notes"), "notes");

		// Hyphens and underscores preserved
		assert_eq!(sanitize_topic_name("my-topic"), "my-topic");
		assert_eq!(sanitize_topic_name("my_topic"), "my_topic");

		// Consecutive special chars collapse
		assert_eq!(sanitize_topic_name("a  b"), "a__b");

		// Leading/trailing underscores trimmed
		assert_eq!(sanitize_topic_name("  topic  "), "topic");
		assert_eq!(sanitize_topic_name("!!!topic!!!"), "topic");
	}
}
