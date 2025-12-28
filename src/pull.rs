use std::{
	collections::BTreeMap,
	io::{Read, Seek, SeekFrom, Write},
	path::PathBuf,
};

use chrono::{DateTime, Utc};
use eyre::{Result, eyre};
use grammers_client::Client;
use grammers_tl_types as tl;
use serde::{Deserialize, Serialize};
use tg::telegram_chat_id;
use tracing::{debug, info, warn};
use v_utils::xdg_state_file;
use xattr::FileExt as _;

use crate::{
	config::{LiveSettings, TopicsMetadata},
	mtproto,
	server::format_message_append_with_sender,
};

/// Sanitize topic name for use as filename: lowercase, replace spaces with underscores
fn sanitize_topic_name(name: &str) -> String {
	name.to_lowercase()
		.chars()
		.map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
		.collect::<String>()
		.trim_matches('_')
		.to_string()
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SyncTimestamps {
	/// Maps "group_id/topic_id" -> last synced message_id
	pub topics: BTreeMap<String, i32>,
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
struct GetChatResponse {
	ok: bool,
	result: Option<TelegramChatInfo>,
	#[serde(default)]
	description: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct TelegramChatInfo {
	#[serde(default)]
	title: Option<String>,
	#[serde(default)]
	is_forum: Option<bool>,
}

/// Fetch group info and discover topics for all configured groups.
/// Creates topic files for each discovered topic in the XDG data home.
/// Uses MTProto API (via grammers) to list all forum topics directly.
pub async fn discover_and_create_topic_files(config: &LiveSettings, bot_token: &str) -> Result<()> {
	let client = reqwest::Client::new();
	let mut topics_metadata = TopicsMetadata::load();

	// First, get group names via Bot API
	for group_id in config.forum_group_ids() {
		let chat_id = telegram_chat_id(group_id);

		let url = format!("https://api.telegram.org/bot{}/getChat", bot_token);
		let res = client.get(&url).query(&[("chat_id", chat_id.to_string())]).send().await?;
		let response: GetChatResponse = res.json().await?;

		if !response.ok {
			warn!("Failed to get chat {}: {:?}", group_id, response.description);
			continue;
		}

		if let Some(chat_info) = response.result {
			let is_forum = chat_info.is_forum.unwrap_or(false);
			if !is_forum {
				warn!("Group {} is not a forum", group_id);
				continue;
			}

			if let Some(title) = chat_info.title {
				let sanitized_name = sanitize_topic_name(&title);
				topics_metadata.set_group_name(group_id, sanitized_name);
				info!("Discovered forum group: {} (id: {})", title, group_id);
			}
		}
	}

	topics_metadata.save()?;

	// Use MTProto to discover all topics (requires user auth)
	info!("Discovering forum topics via MTProto...");
	crate::mtproto::discover_all_topics(config).await?;

	// Reload metadata after MTProto discovery
	let topics_metadata = TopicsMetadata::load();

	// Create topic files for all discovered topics
	create_topic_files(&topics_metadata)?;

	// Run pull to sync messages
	info!("Running pull to sync messages...");
	pull(config, bot_token).await?;

	Ok(())
}

/// Create empty topic files for all topics in metadata
pub fn create_topic_files(metadata: &TopicsMetadata) -> Result<()> {
	let data_dir = crate::server::DATA_DIR.get().unwrap();

	for (group_id, group) in &metadata.groups {
		let default_name = format!("group_{}", group_id);
		let group_name = group.name.as_deref().unwrap_or(&default_name);
		let group_dir = data_dir.join(group_name);

		// Create group directory
		std::fs::create_dir_all(&group_dir)?;

		for (topic_id, topic_name) in &group.topics {
			let file_path = group_dir.join(format!("{}.md", topic_name));

			// Create empty file if it doesn't exist
			if !file_path.exists() {
				std::fs::File::create(&file_path)?;
				info!("Created topic file: {} (topic_id={})", file_path.display(), topic_id);
			}
		}

		info!("Group {} ({}) has {} topics", group_name, group_id, group.topics.len());
	}

	Ok(())
}

/// Run a single pull operation for all configured forum groups using MTProto
pub async fn pull(config: &LiveSettings, _bot_token: &str) -> Result<()> {
	// Check if MTProto credentials are available
	let has_api_id = config.api_id.is_some();
	let has_api_hash = config.api_hash.is_some() || std::env::var("TELEGRAM_API_HASH").is_ok();
	let has_phone = config.phone.is_some() || std::env::var("PHONE_NUMBER_FR").is_ok();

	if !has_api_id || !has_api_hash || !has_phone {
		warn!("MTProto credentials not configured. Cannot pull messages.");
		warn!("Configure api_id, api_hash, and phone in your config file.");
		return Ok(());
	}

	let mut sync_timestamps = SyncTimestamps::load();
	let topics_metadata = TopicsMetadata::load();
	info!("Starting pull via MTProto, loaded sync timestamps for {} topics", sync_timestamps.topics.len());

	// Create MTProto client
	let (client, handle) = mtproto::create_client(config).await?;

	for group_id in config.forum_group_ids() {
		let group = match topics_metadata.groups.get(&group_id) {
			Some(g) => g,
			None => {
				warn!("No metadata for group {}, skipping", group_id);
				continue;
			}
		};

		info!("Pulling messages for group {} ({} topics)...", group_id, group.topics.len());

		// Get InputPeer for this group
		let input_peer = match get_input_peer(&client, group_id).await {
			Ok(p) => p,
			Err(e) => {
				warn!("Could not get peer for group {}: {}", group_id, e);
				continue;
			}
		};

		for (&topic_id, topic_name) in &group.topics {
			let topic_key = format!("{}/{}", group_id, topic_id);
			let last_synced_id = sync_timestamps.topics.get(&topic_key).copied().unwrap_or(0);

			debug!("Pulling topic {} (last_synced_id={})", topic_name, last_synced_id);

			// Fetch messages for this topic
			let messages = match fetch_topic_messages(&client, &input_peer, topic_id as i32, last_synced_id, config.max_messages_per_chat).await {
				Ok(m) => m,
				Err(e) => {
					warn!("Failed to fetch messages for topic {}: {}", topic_name, e);
					continue;
				}
			};

			if messages.is_empty() {
				debug!("No new messages for topic {}", topic_name);
			} else {
				info!("Fetched {} messages for topic {}", messages.len(), topic_name);

				// Find max message ID for updating sync timestamp
				let max_msg_id = messages.iter().map(|m| m.id).max().unwrap_or(last_synced_id);

				// Merge messages into file
				merge_mtproto_messages_to_file(group_id, topic_id, &messages, &topics_metadata).await?;

				// Update sync timestamp
				sync_timestamps.topics.insert(topic_key, max_msg_id);
			}

			// Cleanup tagless messages (optimistic writes that have now been confirmed or failed)
			let file_path = topic_filepath(group_id, topic_id, &topics_metadata);
			if file_path.exists() {
				if let Err(e) = cleanup_tagless_messages(&file_path) {
					warn!("Failed to cleanup tagless messages in {}: {}", file_path.display(), e);
				}
			}
		}
	}

	// Save updated timestamps
	sync_timestamps.save()?;
	info!("Pull complete, saved sync timestamps");

	// Disconnect client
	client.disconnect();
	handle.abort();

	Ok(())
}

/// Get InputPeer from group_id by iterating dialogs
async fn get_input_peer(client: &Client, group_id: u64) -> Result<tl::enums::InputPeer> {
	let chat_id = telegram_chat_id(group_id);
	let mut dialogs = client.iter_dialogs();

	// chat_id is like -1002244305221, we want 2244305221
	let expected_id = if chat_id < 0 {
		let s = chat_id.to_string();
		if s.starts_with("-100") { s[4..].parse::<i64>().unwrap_or(0) } else { chat_id.abs() }
	} else {
		chat_id
	};

	while let Some(dialog) = dialogs.next().await? {
		match &dialog.raw {
			tl::enums::Dialog::Dialog(d) => {
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
			tl::enums::Dialog::Folder(_) => continue,
		}
	}

	Err(eyre!("Could not find channel with id {} in dialogs", group_id))
}

/// A message fetched from MTProto
#[derive(Clone, Debug)]
pub struct FetchedMessage {
	pub id: i32,
	pub date: i32,
	pub text: String,
	pub photo: Option<tl::types::Photo>,
	/// True if the message was sent by the authenticated user (not a bot)
	pub is_outgoing: bool,
}

/// Fetch messages from a forum topic using MTProto
async fn fetch_topic_messages(client: &Client, input_peer: &tl::enums::InputPeer, topic_id: i32, min_id: i32, limit: usize) -> Result<Vec<FetchedMessage>> {
	let mut messages = Vec::new();
	let mut offset_id = 0;

	loop {
		// For forum topics, use GetReplies with msg_id = topic_id (the topic creation message)
		let request = tl::functions::messages::GetReplies {
			peer: input_peer.clone(),
			msg_id: topic_id,
			offset_id,
			offset_date: 0,
			add_offset: 0,
			limit: 100,
			max_id: 0,
			min_id,
			hash: 0,
		};

		let result = client.invoke(&request).await?;

		let fetched_messages = match result {
			tl::enums::messages::Messages::Messages(m) => m.messages,
			tl::enums::messages::Messages::Slice(m) => m.messages,
			tl::enums::messages::Messages::ChannelMessages(m) => m.messages,
			tl::enums::messages::Messages::NotModified(_) => break,
		};

		if fetched_messages.is_empty() {
			break;
		}

		let batch_count = fetched_messages.len();

		for msg in fetched_messages {
			match msg {
				tl::enums::Message::Message(m) => {
					// Skip service messages (topic created, etc.)
					if m.message.is_empty() && m.media.is_none() {
						continue;
					}

					// Extract photo if present
					let photo = m.media.as_ref().and_then(|media| match media {
						tl::enums::MessageMedia::Photo(p) => p.photo.as_ref().and_then(|photo| match photo {
							tl::enums::Photo::Photo(ph) => Some(ph.clone()),
							_ => None,
						}),
						_ => None,
					});

					messages.push(FetchedMessage {
						id: m.id,
						date: m.date,
						text: m.message.clone(),
						photo,
						is_outgoing: m.out,
					});
				}
				tl::enums::Message::Service(_) => continue,
				tl::enums::Message::Empty(_) => continue,
			}
		}

		// Update offset for pagination - use the smallest ID we've seen
		if let Some(min_seen) = messages.iter().map(|m| m.id).min() {
			if min_seen == offset_id {
				break; // No progress, stop
			}
			offset_id = min_seen;
		} else {
			break;
		}

		// Check if we've reached the limit or got all messages
		if messages.len() >= limit || batch_count < 100 {
			break;
		}
	}

	// Sort by date (oldest first)
	messages.sort_by_key(|m| m.date);

	// Limit to most recent messages
	if messages.len() > limit {
		let skip_count = messages.len() - limit;
		messages = messages.into_iter().skip(skip_count).collect();
	}

	Ok(messages)
}

/// Merge MTProto messages into a topic file
async fn merge_mtproto_messages_to_file(group_id: u64, topic_id: u64, messages: &[FetchedMessage], metadata: &TopicsMetadata) -> Result<()> {
	ensure_topic_dir(group_id, metadata)?;
	let chat_filepath = topic_filepath(group_id, topic_id, metadata);

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
	let truncate_to = std::cmp::min(trimmed_len + 1, file_contents.len());
	file.set_len(truncate_to as u64)?;
	file.seek(SeekFrom::End(0))?;

	let mut last_write = last_write_datetime;

	for msg in messages {
		let msg_time = DateTime::from_timestamp(msg.date as i64, 0).unwrap_or_else(Utc::now);
		// is_outgoing=true means sent by the authenticated user, false means bot or other users
		let sender = if msg.is_outgoing { "user" } else { "bot" };

		// Handle photo messages (just note them for now, TODO: implement download)
		if msg.photo.is_some() {
			let content = if msg.text.is_empty() { "[photo]".to_string() } else { format!("[photo]\n{}", msg.text) };
			let formatted = format_message_append_with_sender(&content, last_write, msg_time, Some(msg.id), Some(sender));
			file.write_all(formatted.as_bytes())?;
			last_write = Some(msg_time);
		} else if !msg.text.is_empty() {
			let formatted = format_message_append_with_sender(&msg.text, last_write, msg_time, Some(msg.id), Some(sender));
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

/// Remove tagless message lines from a topic file
/// Tagless messages are optimistic writes that should be replaced by tagged versions from TG
/// after sync. Lines without tags are removed, keeping:
/// - Date headers (lines starting with "## ")
/// - Empty lines
/// - Lines with message ID tags (<!-- msg:N -->)
fn cleanup_tagless_messages(file_path: &std::path::Path) -> Result<()> {
	use regex::Regex;

	let content = std::fs::read_to_string(file_path)?;
	let msg_id_re = Regex::new(r"<!-- msg:\d+ -->").unwrap();

	let mut new_lines: Vec<&str> = Vec::new();
	let mut removed_count = 0;

	for line in content.lines() {
		let trimmed = line.trim();

		// Keep empty lines
		if trimmed.is_empty() {
			new_lines.push(line);
			continue;
		}

		// Keep date headers
		if trimmed.starts_with("## ") {
			new_lines.push(line);
			continue;
		}

		// Keep lines with message ID tags
		if msg_id_re.is_match(line) {
			new_lines.push(line);
			continue;
		}

		// This is a tagless message line - remove it
		removed_count += 1;
		debug!("Removing tagless line: {}", trimmed);
	}

	if removed_count > 0 {
		info!("Cleaned up {} tagless message(s) from {}", removed_count, file_path.display());
		let new_content = new_lines.join("\n");
		// Preserve trailing newline if original had one
		let final_content = if content.ends_with('\n') && !new_content.ends_with('\n') {
			format!("{}\n", new_content)
		} else {
			new_content
		};
		std::fs::write(file_path, final_content)?;
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_sanitize_topic_name() {
		assert_eq!(sanitize_topic_name("Journal"), "journal");
		assert_eq!(sanitize_topic_name("My Topic"), "my_topic");
		assert_eq!(sanitize_topic_name("🔥 Hot Takes"), "hot_takes");
		assert_eq!(sanitize_topic_name("my-topic"), "my-topic");
	}

	#[test]
	fn test_cleanup_tagless_messages() {
		use std::io::Write as _;

		let temp_dir = std::env::temp_dir();
		let test_file = temp_dir.join("test_cleanup.md");

		// Create test file with mixed content
		let content = r#"## Jan 03
Hello with tag <!-- msg:123 -->
This is tagless and should be removed
. Another tagged message <!-- msg:456 -->

## Jan 04
Untagged message
Tagged message <!-- msg:789 -->
"#;
		let mut file = std::fs::File::create(&test_file).unwrap();
		file.write_all(content.as_bytes()).unwrap();

		// Run cleanup
		cleanup_tagless_messages(&test_file).unwrap();

		// Read result
		let result = std::fs::read_to_string(&test_file).unwrap();

		// Verify: tagless messages removed, headers/tagged/empty preserved
		assert!(result.contains("## Jan 03"));
		assert!(result.contains("Hello with tag <!-- msg:123 -->"));
		assert!(!result.contains("This is tagless"));
		assert!(result.contains(". Another tagged message <!-- msg:456 -->"));
		assert!(result.contains("## Jan 04"));
		assert!(!result.contains("Untagged message"));
		assert!(result.contains("Tagged message <!-- msg:789 -->"));

		// Cleanup
		std::fs::remove_file(&test_file).ok();
	}
}
