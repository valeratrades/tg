use eyre::{Result, eyre};
use grammers_client::Client;
use grammers_tl_types as tl;
use jiff::Timestamp;
use serde::Deserialize;
use tg::telegram_chat_id;
use tracing::{debug, info, warn};

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

/// Extract the highest message ID from a file's content by parsing `<!-- msg:ID ... -->` tags
fn extract_max_message_id(file_path: &std::path::Path) -> i32 {
	let content = match std::fs::read_to_string(file_path) {
		Ok(c) => c,
		Err(_) => return 0,
	};
	extract_max_message_id_from_content(&content)
}

fn extract_max_message_id_from_content(content: &str) -> i32 {
	use regex::Regex;

	let msg_id_re = Regex::new(r"<!-- msg:(\d+)").unwrap();
	let mut max_id = 0;

	for cap in msg_id_re.captures_iter(content) {
		if let Some(id) = cap.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
			max_id = max_id.max(id);
		}
	}

	max_id
}

fn parse_month(s: &str) -> Option<i8> {
	match s {
		"Jan" => Some(1),
		"Feb" => Some(2),
		"Mar" => Some(3),
		"Apr" => Some(4),
		"May" => Some(5),
		"Jun" => Some(6),
		"Jul" => Some(7),
		"Aug" => Some(8),
		"Sep" => Some(9),
		"Oct" => Some(10),
		"Nov" => Some(11),
		"Dec" => Some(12),
		_ => None,
	}
}

/// Backfill years for date headers that don't have them.
/// Uses the first header WITH a year as anchor, then walks backwards inferring years.
/// If month increases going backwards, we crossed a year boundary.
///
/// Note: This can incorrectly merge months from different years if user had exactly
/// 11 months of inactivity (e.g., Jan 2023 and Jan 2024 would both become Jan 2024).
/// This edge case is rare and acceptable.
fn backfill_date_header_years(content: &str) -> String {
	use regex::Regex;

	let date_header_re = Regex::new(r"^(## )([A-Za-z]{3}) (\d{1,2})(, (\d{4}))?$").unwrap();

	// First pass: collect all date headers with their line indices and parsed info
	struct DateHeader {
		line_idx: usize,
		month: i8,
		day: i8,
		year: Option<i16>,
		full_line: String,
	}

	let lines: Vec<&str> = content.lines().collect();
	let mut headers: Vec<DateHeader> = Vec::new();

	for (idx, line) in lines.iter().enumerate() {
		let trimmed = line.trim();
		if let Some(caps) = date_header_re.captures(trimmed) {
			let month_str = caps.get(2).map(|m| m.as_str()).unwrap_or("");
			let day: i8 = caps.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(1);
			let year: Option<i16> = caps.get(5).and_then(|m| m.as_str().parse().ok());

			if let Some(month) = parse_month(month_str) {
				headers.push(DateHeader {
					line_idx: idx,
					month,
					day,
					year,
					full_line: trimmed.to_string(),
				});
			}
		}
	}

	if headers.is_empty() {
		return content.to_string();
	}

	// Find first header with a year (anchor point)
	let anchor_idx = headers.iter().position(|h| h.year.is_some());

	let Some(anchor_idx) = anchor_idx else {
		// No headers have years - can't infer anything
		// Use current year for all (will be fixed on next pull with new messages)
		let current_year = jiff::Zoned::now().year();
		let mut result_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
		for header in &headers {
			let new_line = format!("## {} {:02}, {}", month_name(header.month), header.day, current_year);
			result_lines[header.line_idx] = new_line;
		}
		return result_lines.join("\n");
	};

	// Walk backwards from anchor, inferring years
	let mut inferred_years: Vec<i16> = vec![0; headers.len()];
	inferred_years[anchor_idx] = headers[anchor_idx].year.unwrap();

	// Backward pass
	for i in (0..anchor_idx).rev() {
		let next_year = inferred_years[i + 1];
		let next_month = headers[i + 1].month;
		let curr_month = headers[i].month;

		// If current month > next month, we crossed a year boundary going backwards
		inferred_years[i] = if curr_month > next_month { next_year - 1 } else { next_year };
	}

	// Forward pass from anchor
	for i in (anchor_idx + 1)..headers.len() {
		let prev_year = inferred_years[i - 1];
		let prev_month = headers[i - 1].month;
		let curr_month = headers[i].month;

		// If current month < previous month, we crossed a year boundary going forward
		inferred_years[i] = if curr_month < prev_month { prev_year + 1 } else { prev_year };
	}

	// Build result with updated headers
	let mut result_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
	for (i, header) in headers.iter().enumerate() {
		if header.year.is_none() {
			let new_line = format!("## {} {:02}, {}", month_name(header.month), header.day, inferred_years[i]);
			info!("Backfilled year: {} -> {}", header.full_line, new_line);
			result_lines[header.line_idx] = new_line;
		}
	}

	result_lines.join("\n")
}

fn month_name(month: i8) -> &'static str {
	match month {
		1 => "Jan",
		2 => "Feb",
		3 => "Mar",
		4 => "Apr",
		5 => "May",
		6 => "Jun",
		7 => "Jul",
		8 => "Aug",
		9 => "Sep",
		10 => "Oct",
		11 => "Nov",
		12 => "Dec",
		_ => "???",
	}
}

/// Backfill date header years in a file if any headers are missing years
fn backfill_file_date_headers(file_path: &std::path::Path) -> Result<()> {
	let content = std::fs::read_to_string(file_path)?;

	// Check if any headers need backfilling
	let has_headers_without_years = content.lines().any(|line| {
		let trimmed = line.trim();
		// Match headers like "## Dec 25" (without year)
		trimmed.starts_with("## ")
			&& trimmed.len() > 3
			&& !trimmed.contains(',') // no comma means no year
			&& trimmed.chars().skip(3).take(3).all(|c| c.is_alphabetic())
	});

	if !has_headers_without_years {
		return Ok(());
	}

	let backfilled = backfill_date_header_years(&content);
	if backfilled != content {
		std::fs::write(file_path, backfilled)?;
	}

	Ok(())
}

/// Extract the last date header from file content and parse it as a date
/// Date headers look like `## Dec 25, 2024` or `## Jan 03`
/// Only returns dates from headers WITH explicit years (after backfill)
fn extract_last_date_from_content(content: &str) -> Option<jiff::civil::Date> {
	use regex::Regex;

	// Only match date headers with explicit years
	let date_header_re = Regex::new(r"^## ([A-Za-z]{3}) (\d{1,2}), (\d{4})$").unwrap();

	let mut last_date: Option<jiff::civil::Date> = None;

	for line in content.lines() {
		let trimmed = line.trim();
		if let Some(caps) = date_header_re.captures(trimmed) {
			let month_str = caps.get(1).map(|m| m.as_str()).unwrap_or("");
			let day: i8 = caps.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(1);
			let year: i16 = caps.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);

			if let Some(month) = parse_month(month_str) {
				if let Ok(date) = jiff::civil::Date::new(year, month, day) {
					last_date = Some(date);
				}
			}
		}
	}

	last_date
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
	let cfg = config.config()?;
	for group_id in cfg.forum_group_ids() {
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
	let cfg = config.config()?;
	let has_api_id = cfg.api_id.is_some();
	let has_api_hash = cfg.api_hash.is_some() || std::env::var("TELEGRAM_API_HASH").is_ok();
	let has_phone = cfg.phone.is_some() || std::env::var("PHONE_NUMBER_FR").is_ok();

	if !has_api_id || !has_api_hash || !has_phone {
		warn!("MTProto credentials not configured. Cannot pull messages.");
		warn!("Configure api_id, api_hash, and phone in your config file.");
		return Ok(());
	}

	let topics_metadata = TopicsMetadata::load();
	info!("Starting pull via MTProto...");

	// Create MTProto client
	let (client, handle) = mtproto::create_client(config).await?;

	for group_id in cfg.forum_group_ids() {
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
			let file_path = topic_filepath(group_id, topic_id, &topics_metadata);

			// Always backfill date header years if needed (runs even without new messages)
			if file_path.exists() {
				if let Err(e) = backfill_file_date_headers(&file_path) {
					warn!("Failed to backfill date headers in {}: {}", file_path.display(), e);
				}
			}

			// Extract last message ID directly from the file content
			let last_synced_id = extract_max_message_id(&file_path);

			debug!("Pulling topic {} (last_synced_id={} from file)", topic_name, last_synced_id);

			// Fetch messages for this topic
			let messages = match fetch_topic_messages(&client, &input_peer, topic_id as i32, last_synced_id, cfg.max_messages_per_chat).await {
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

				// Merge messages into file
				merge_mtproto_messages_to_file(group_id, topic_id, &messages, &topics_metadata).await?;
			}

			// Cleanup tagless messages (optimistic writes that have now been confirmed or failed)
			if file_path.exists() {
				let before = std::fs::read_to_string(&file_path).map(|s| s.lines().count()).unwrap_or(0);
				if let Err(e) = cleanup_tagless_messages(&file_path) {
					warn!("Failed to cleanup tagless messages in {}: {}", file_path.display(), e);
				}
				let after = std::fs::read_to_string(&file_path).map(|s| s.lines().count()).unwrap_or(0);
				if before != after {
					debug!("Cleanup changed {} from {} to {} lines", file_path.display(), before, after);
				}
			}
		}
	}

	info!("Pull complete");

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
		if let Some(stripped) = s.strip_prefix("-100") {
			stripped.parse::<i64>().unwrap_or(0)
		} else {
			chat_id.abs()
		}
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

					debug!(id = m.id, out = m.out, text = %m.message, "raw message");

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

/// Check if message IDs in file content are strictly increasing
/// Returns true if valid, false if reordering is needed
fn are_message_ids_increasing(content: &str) -> bool {
	use regex::Regex;

	let msg_id_re = Regex::new(r"<!-- msg:(\d+)").unwrap();
	let mut prev_id: Option<i32> = None;

	for cap in msg_id_re.captures_iter(content) {
		if let Some(id) = cap.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
			if let Some(prev) = prev_id {
				if id <= prev {
					return false;
				}
			}
			prev_id = Some(id);
		}
	}

	true
}

/// A message block extracted from file content for reordering
#[derive(Debug)]
struct MessageBlock {
	/// The message ID from the tag
	id: i32,
	/// The full content lines of this message (may span multiple lines for code blocks)
	lines: Vec<String>,
}

/// Reorder messages in file content so IDs are strictly increasing.
/// Preserves date headers and places messages under the correct date based on their original position.
/// Returns the reordered content.
fn reorder_messages_by_id(content: &str) -> String {
	use regex::Regex;

	let msg_id_re = Regex::new(r"<!-- msg:(\d+)").unwrap();
	let date_header_re = Regex::new(r"^## [A-Za-z]{3} \d{1,2}(, \d{4})?$").unwrap();

	// Parse content into sections: date headers and message blocks
	#[derive(Debug)]
	enum Section {
		DateHeader(String),
		Message(MessageBlock),
		Empty,
	}

	let lines: Vec<&str> = content.lines().collect();
	let mut sections: Vec<Section> = Vec::new();
	let mut i = 0;

	while i < lines.len() {
		let line = lines[i];
		let trimmed = line.trim();

		// Empty line
		if trimmed.is_empty() {
			sections.push(Section::Empty);
			i += 1;
			continue;
		}

		// Date header
		if date_header_re.is_match(trimmed) {
			sections.push(Section::DateHeader(line.to_string()));
			i += 1;
			continue;
		}

		// Check if this is a message with ID tag on this line
		if let Some(cap) = msg_id_re.captures(line) {
			if let Some(id) = cap.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
				sections.push(Section::Message(MessageBlock { id, lines: vec![line.to_string()] }));
				i += 1;
				continue;
			}
		}

		// Check for code block start (5+ backticks) - multiline message
		if trimmed.contains("`````") {
			let mut block_lines = vec![line.to_string()];
			i += 1;

			// Find closing fence
			while i < lines.len() {
				let next_line = lines[i];
				block_lines.push(next_line.to_string());

				// Check if this is a closing fence (line with only backticks)
				let next_trimmed = next_line.trim();
				if next_trimmed.chars().all(|c| c == '`') && next_trimmed.len() >= 5 {
					// Pure backtick fence - tag should be on the next line
					i += 1;
					if i < lines.len() {
						let tag_line = lines[i];
						if let Some(cap) = msg_id_re.captures(tag_line) {
							if let Some(id) = cap.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
								block_lines.push(tag_line.to_string());
								sections.push(Section::Message(MessageBlock { id, lines: block_lines }));
								i += 1;
							}
						}
					}
					break;
				} else if next_line.contains("`````") {
					// Legacy format: closing fence has the tag on the same line
					if let Some(cap) = msg_id_re.captures(next_line) {
						if let Some(id) = cap.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
							sections.push(Section::Message(MessageBlock { id, lines: block_lines }));
						}
					}
					i += 1;
					break;
				}
				i += 1;
			}
			continue;
		}

		// Unknown line (shouldn't happen after cleanup, but skip it)
		i += 1;
	}

	// Extract messages and their associated date headers
	// We'll collect (date_header_index, message) pairs
	let mut date_indexed_messages: Vec<(usize, MessageBlock)> = Vec::new();
	let mut current_date_idx: Option<usize> = None;

	for (idx, section) in sections.iter().enumerate() {
		match section {
			Section::DateHeader(_) => current_date_idx = Some(idx),
			Section::Message(msg) => {
				date_indexed_messages.push((
					current_date_idx.unwrap_or(0),
					MessageBlock {
						id: msg.id,
						lines: msg.lines.clone(),
					},
				));
			}
			Section::Empty => {}
		}
	}

	// Sort messages by ID
	date_indexed_messages.sort_by_key(|(_, msg)| msg.id);

	// Collect date headers in order
	let date_headers: Vec<(usize, String)> = sections
		.iter()
		.enumerate()
		.filter_map(|(idx, s)| if let Section::DateHeader(h) = s { Some((idx, h.clone())) } else { None })
		.collect();

	// Rebuild content: for each date header, emit messages that were originally under it (now sorted)
	let mut result_lines: Vec<String> = Vec::new();
	let mut used_messages: std::collections::HashSet<i32> = std::collections::HashSet::new();

	for (date_idx, date_header) in &date_headers {
		// Find next date header index (or end)
		let next_date_idx = date_headers.iter().find(|(idx, _)| idx > date_idx).map(|(idx, _)| *idx).unwrap_or(usize::MAX);

		// Get messages that belong to this date section (by original position)
		let section_messages: Vec<&MessageBlock> = date_indexed_messages
			.iter()
			.filter(|(orig_date_idx, msg)| *orig_date_idx >= *date_idx && *orig_date_idx < next_date_idx && !used_messages.contains(&msg.id))
			.map(|(_, msg)| msg)
			.collect();

		if section_messages.is_empty() {
			// Skip empty date sections
			continue;
		}

		// Add date header
		result_lines.push(date_header.clone());

		// Add messages in sorted order
		for msg in section_messages {
			for line in &msg.lines {
				result_lines.push(line.clone());
			}
			used_messages.insert(msg.id);
		}

		// Add blank line between sections
		result_lines.push(String::new());
	}

	// Handle any messages without a date header (shouldn't happen normally)
	let remaining: Vec<&MessageBlock> = date_indexed_messages.iter().filter(|(_, msg)| !used_messages.contains(&msg.id)).map(|(_, msg)| msg).collect();

	if !remaining.is_empty() {
		for msg in remaining {
			for line in &msg.lines {
				result_lines.push(line.clone());
			}
		}
	}

	// Remove trailing empty lines
	while result_lines.last().map(|l| l.is_empty()).unwrap_or(false) {
		result_lines.pop();
	}

	let result = result_lines.join("\n");
	if !result.is_empty() { format!("{}\n", result) } else { result }
}

/// Merge MTProto messages into a topic file
async fn merge_mtproto_messages_to_file(group_id: u64, topic_id: u64, messages: &[FetchedMessage], metadata: &TopicsMetadata) -> Result<()> {
	ensure_topic_dir(group_id, metadata)?;
	let chat_filepath = topic_filepath(group_id, topic_id, metadata);
	debug!("Merging {} messages to {}", messages.len(), chat_filepath.display());

	let mut file_contents = std::fs::read_to_string(&chat_filepath).unwrap_or_default();

	// Check if message IDs are strictly increasing; if not, reorder them
	if !are_message_ids_increasing(&file_contents) {
		warn!("Message IDs not strictly increasing in {}, reordering...", chat_filepath.display());
		file_contents = reorder_messages_by_id(&file_contents);
		// Write the reordered content back
		std::fs::write(&chat_filepath, &file_contents)?;
		info!("Reordered messages in {}", chat_filepath.display());
	}

	// Backfill years for old date headers that don't have them
	// This uses the first header WITH a year as anchor and infers backwards
	let file_contents = backfill_date_header_years(&file_contents);

	// Write backfilled content and prepare for appending
	let trimmed_content = file_contents.trim_end();
	let file_contents_with_newline = if trimmed_content.is_empty() { String::new() } else { format!("{}\n", trimmed_content) };

	// Extract the last date from existing file content (from date headers like `## Dec 25, 2024`)
	// Only use headers with explicit years (after backfill, all should have years)
	let last_date_in_file = extract_last_date_from_content(&file_contents_with_newline);
	let last_write_datetime: Option<Timestamp> = last_date_in_file.and_then(|date| {
		// Convert date to timestamp at end of day (23:59:59) so messages on the same day
		// don't trigger a new date header
		date.at(23, 59, 59, 0).to_zoned(jiff::tz::TimeZone::UTC).ok().map(|z| z.timestamp())
	});

	let mut last_write = last_write_datetime;
	let mut new_content = file_contents_with_newline;

	for msg in messages {
		let msg_time = Timestamp::from_second(msg.date as i64).unwrap_or_else(|_| Timestamp::now());
		// is_outgoing=true means sent by the authenticated user, false means bot or other users
		let sender = if msg.is_outgoing { "user" } else { "bot" };

		// Handle photo messages (just note them for now, TODO: implement download)
		if msg.photo.is_some() {
			let content = if msg.text.is_empty() { "[photo]".to_string() } else { format!("[photo]\n{}", msg.text) };
			let formatted = format_message_append_with_sender(&content, last_write, msg_time, Some(msg.id), Some(sender));
			new_content.push_str(&formatted);
			last_write = Some(msg_time);
		} else if !msg.text.is_empty() {
			let formatted = format_message_append_with_sender(&msg.text, last_write, msg_time, Some(msg.id), Some(sender));
			new_content.push_str(&formatted);
			last_write = Some(msg_time);
		}
	}

	// Write the complete file
	std::fs::write(&chat_filepath, &new_content)?;

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

/// Clean up a topic file:
/// 1. Remove tagless message lines (optimistic writes that should be replaced by tagged versions)
/// 2. Remove empty date sections (date headers followed by only empty lines or another header)
/// 3. Collapse multiple consecutive empty lines into one
fn cleanup_tagless_messages(file_path: &std::path::Path) -> Result<()> {
	use regex::Regex;

	let content = std::fs::read_to_string(file_path)?;
	// Match both old format <!-- msg:123 --> and new format <!-- msg:123 sender -->
	let msg_id_re = Regex::new(r"<!-- msg:\d+").unwrap();
	// Match both old format (## Jan 03) and new format (## Jan 03, 2025)
	let date_header_re = Regex::new(r"^## [A-Za-z]{3} \d{1,2}(, \d{4})?$").unwrap();

	// First pass: remove tagless message lines
	let mut filtered_lines: Vec<&str> = Vec::new();
	let mut removed_count = 0;
	let mut in_code_block = false;

	for line in content.lines() {
		let trimmed = line.trim();

		// Track code block state (5+ backtick blocks used for complex messages)
		// Check for 5 or more backticks (our minimum fence length)
		if trimmed.contains("`````") {
			in_code_block = !in_code_block;
			filtered_lines.push(line);
			continue;
		}

		// Keep all lines inside code blocks
		if in_code_block {
			filtered_lines.push(line);
			continue;
		}

		// Keep empty lines
		if trimmed.is_empty() {
			filtered_lines.push(line);
			continue;
		}

		// Keep date headers
		if date_header_re.is_match(trimmed) {
			filtered_lines.push(line);
			continue;
		}

		// Keep lines with message ID tags
		if msg_id_re.is_match(line) {
			filtered_lines.push(line);
			continue;
		}

		// This is a tagless message line - remove it
		removed_count += 1;
		debug!(line = &trimmed[..trimmed.len().min(80)], "cleanup: removing tagless line");
	}

	// Second pass: remove empty date sections and collapse empty lines
	let mut final_lines: Vec<&str> = Vec::new();
	let mut i = 0;
	while i < filtered_lines.len() {
		let line = filtered_lines[i];
		let trimmed = line.trim();

		// Skip consecutive empty lines (keep only one)
		if trimmed.is_empty() {
			if final_lines.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
				i += 1;
				continue;
			}
			final_lines.push(line);
			i += 1;
			continue;
		}

		// Check if this is a date header
		if date_header_re.is_match(trimmed) {
			// Look ahead to see if there's any content before the next header or end
			let mut has_content = false;
			let mut j = i + 1;
			while j < filtered_lines.len() {
				let next_trimmed = filtered_lines[j].trim();
				if next_trimmed.is_empty() {
					j += 1;
					continue;
				}
				if date_header_re.is_match(next_trimmed) {
					// Hit another date header without content
					break;
				}
				// Found content
				has_content = true;
				break;
			}

			if has_content {
				final_lines.push(line);
			} else {
				removed_count += 1;
				debug!("Removing empty date section: {}", trimmed);
			}
			i += 1;
			continue;
		}

		// Regular content line
		final_lines.push(line);
		i += 1;
	}

	// Remove trailing empty lines
	while final_lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
		final_lines.pop();
	}

	if removed_count > 0 {
		info!("Cleaned up {} lines from {}", removed_count, file_path.display());
		let new_content = final_lines.join("\n");
		let final_content = if !new_content.is_empty() { format!("{}\n", new_content) } else { new_content };
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

	#[test]
	fn test_cleanup_empty_date_sections() {
		use std::io::Write as _;

		let temp_dir = std::env::temp_dir();
		let test_file = temp_dir.join("test_cleanup_empty.md");

		// Create test file with empty date sections
		let content = r#"## Dec 25
Message on Dec 25 <!-- msg:100 -->

## Dec 26

## Dec 27

## Dec 27

## Dec 28
Message on Dec 28 <!-- msg:101 -->
"#;
		let mut file = std::fs::File::create(&test_file).unwrap();
		file.write_all(content.as_bytes()).unwrap();

		// Run cleanup
		cleanup_tagless_messages(&test_file).unwrap();

		// Read result
		let result = std::fs::read_to_string(&test_file).unwrap();

		// Verify: empty date sections removed
		assert!(result.contains("## Dec 25"));
		assert!(result.contains("Message on Dec 25"));
		assert!(!result.contains("## Dec 26")); // Empty section removed
		assert!(!result.contains("## Dec 27")); // Empty sections removed
		assert!(result.contains("## Dec 28"));
		assert!(result.contains("Message on Dec 28"));

		// Count occurrences of date headers
		let dec_28_count = result.matches("## Dec 28").count();
		assert_eq!(dec_28_count, 1, "Should have exactly one Dec 28 header");

		// Cleanup
		std::fs::remove_file(&test_file).ok();
	}

	#[test]
	fn test_cleanup_preserves_code_block_content() {
		use std::io::Write as _;

		let temp_dir = std::env::temp_dir();
		let test_file = temp_dir.join("test_cleanup_codeblock.md");

		// Create test file with 5-backtick code block (multiline message format)
		// Tag is on same line as closing fence, consistent with regular messages
		let content = r#"## Dec 27
some message <!-- msg:100 -->

`````md
TODO: integrate resume into the site

# Impl details
while at it, add photo and general info on yourself at the top of /contact

// will no longer need separate repo for it
````` <!-- msg:123 user -->
next message <!-- msg:124 bot -->
"#;
		let mut file = std::fs::File::create(&test_file).unwrap();
		file.write_all(content.as_bytes()).unwrap();

		// Run cleanup
		cleanup_tagless_messages(&test_file).unwrap();

		// Read result
		let result = std::fs::read_to_string(&test_file).unwrap();

		// Verify: all content inside code block is preserved
		assert!(result.contains("`````md"), "Opening fence should be preserved");
		assert!(result.contains("TODO: integrate resume into the site"), "Content line 1 should be preserved");
		assert!(result.contains("# Impl details"), "Header inside block should be preserved");
		assert!(result.contains("while at it, add photo"), "Content line 2 should be preserved");
		assert!(result.contains("// will no longer need"), "Content line 3 should be preserved");
		assert!(result.contains("````` <!-- msg:123 user -->"), "Closing fence with tag should be preserved");
		assert!(result.contains("next message <!-- msg:124 bot -->"), "Next message should be preserved");

		// Cleanup
		std::fs::remove_file(&test_file).ok();
	}

	#[test]
	fn test_cleanup_preserves_new_codeblock_format() {
		use std::io::Write as _;

		let temp_dir = std::env::temp_dir();
		let test_file = temp_dir.join("test_cleanup_new_codeblock.md");

		// Create test file with new format: closing fence on its own line, tag on next line
		let content = r#"## Dec 27
some message <!-- msg:100 -->

`````md
Multi-line message

With paragraph
`````
<!-- msg:123 user -->
next message <!-- msg:124 bot -->
"#;
		let mut file = std::fs::File::create(&test_file).unwrap();
		file.write_all(content.as_bytes()).unwrap();

		// Run cleanup
		cleanup_tagless_messages(&test_file).unwrap();

		// Read result
		let result = std::fs::read_to_string(&test_file).unwrap();

		// Verify: all content is preserved
		assert!(result.contains("`````md"), "Opening fence should be preserved");
		assert!(result.contains("Multi-line message"), "Content should be preserved");
		assert!(result.contains("With paragraph"), "Content should be preserved");
		// Check that the closing fence is on its own line (pure backticks)
		assert!(result.lines().any(|l| l.trim() == "`````"), "Pure closing fence should be preserved");
		assert!(result.contains("<!-- msg:123 user -->"), "Tag should be preserved");
		assert!(result.contains("next message <!-- msg:124 bot -->"), "Next message should be preserved");

		// Cleanup
		std::fs::remove_file(&test_file).ok();
	}

	#[test]
	fn test_extract_last_date_from_content() {
		// Test with year
		let content = "## Dec 25, 2024\nmessage\n## Dec 26, 2024\nmessage";
		let date = extract_last_date_from_content(content);
		assert_eq!(date, Some(jiff::civil::Date::new(2024, 12, 26).unwrap()));

		// Test without year - should return None (only explicit years are used)
		let content = "## Jan 03\nmessage";
		let date = extract_last_date_from_content(content);
		assert_eq!(date, None);

		// Test empty content
		let date = extract_last_date_from_content("");
		assert_eq!(date, None);

		// Test content with no date headers
		let content = "just some message <!-- msg:123 -->";
		let date = extract_last_date_from_content(content);
		assert_eq!(date, None);
	}

	#[test]
	fn test_backfill_date_header_years() {
		// Test backfilling with anchor in middle
		let content = "## Nov 15\nmsg\n## Dec 25, 2024\nmsg\n## Jan 03\nmsg";
		let result = backfill_date_header_years(content);
		assert!(result.contains("## Nov 15, 2024"));
		assert!(result.contains("## Dec 25, 2024"));
		assert!(result.contains("## Jan 03, 2025")); // crossed year boundary

		// Test year boundary going backwards (Dec -> Nov means same year)
		let content = "## Oct 01\nmsg\n## Nov 15\nmsg\n## Dec 25, 2024\nmsg";
		let result = backfill_date_header_years(content);
		assert!(result.contains("## Oct 01, 2024"));
		assert!(result.contains("## Nov 15, 2024"));
		assert!(result.contains("## Dec 25, 2024"));

		// Test no anchor (no years at all) - should use current year
		let content = "## Jan 03\nmsg\n## Feb 15\nmsg";
		let result = backfill_date_header_years(content);
		let current_year = jiff::Zoned::now().year();
		assert!(result.contains(&format!("## Jan 03, {}", current_year)));
		assert!(result.contains(&format!("## Feb 15, {}", current_year)));
	}

	#[test]
	fn test_are_message_ids_increasing() {
		// Valid: strictly increasing
		let content = "msg <!-- msg:100 -->\nmsg <!-- msg:200 -->\nmsg <!-- msg:300 -->";
		assert!(are_message_ids_increasing(content));

		// Invalid: not increasing
		let content = "msg <!-- msg:200 -->\nmsg <!-- msg:100 -->";
		assert!(!are_message_ids_increasing(content));

		// Invalid: equal IDs
		let content = "msg <!-- msg:100 -->\nmsg <!-- msg:100 -->";
		assert!(!are_message_ids_increasing(content));

		// Valid: empty content
		assert!(are_message_ids_increasing(""));

		// Valid: single message
		let content = "msg <!-- msg:100 -->";
		assert!(are_message_ids_increasing(content));
	}

	#[test]
	fn test_reorder_messages_by_id() {
		// Test basic reordering
		let content = "## Dec 25, 2024\nmsg B <!-- msg:200 -->\nmsg A <!-- msg:100 -->\nmsg C <!-- msg:300 -->\n";
		let result = reorder_messages_by_id(content);
		assert!(are_message_ids_increasing(&result));
		// Check that messages are in correct order
		let pos_a = result.find("msg:100").unwrap();
		let pos_b = result.find("msg:200").unwrap();
		let pos_c = result.find("msg:300").unwrap();
		assert!(pos_a < pos_b);
		assert!(pos_b < pos_c);

		// Test with multiple date sections
		let content = "## Dec 25, 2024\nmsg 2 <!-- msg:200 -->\nmsg 1 <!-- msg:100 -->\n\n## Dec 26, 2024\nmsg 4 <!-- msg:400 -->\nmsg 3 <!-- msg:300 -->\n";
		let result = reorder_messages_by_id(content);
		assert!(are_message_ids_increasing(&result));
		// Verify structure preserved
		assert!(result.contains("## Dec 25, 2024"));
		assert!(result.contains("## Dec 26, 2024"));

		// Test with code blocks (multiline messages) - legacy format
		let content = "## Dec 27, 2024\n`````md\ncode content\n````` <!-- msg:200 user -->\nsimple msg <!-- msg:100 bot -->\n";
		let result = reorder_messages_by_id(content);
		assert!(are_message_ids_increasing(&result));
		// Code block content should be preserved
		assert!(result.contains("code content"));

		// Test with new format code blocks (tag on separate line)
		let content = "## Dec 27, 2024\n`````md\nnew format content\n`````\n<!-- msg:200 user -->\nsimple msg <!-- msg:100 bot -->\n";
		let result = reorder_messages_by_id(content);
		assert!(are_message_ids_increasing(&result));
		// Code block content should be preserved
		assert!(result.contains("new format content"));
	}

	#[test]
	fn test_extract_max_message_id_from_content() {
		let content = "msg <!-- msg:100 -->\nmsg <!-- msg:200 -->\nmsg <!-- msg:150 -->";
		assert_eq!(extract_max_message_id_from_content(content), 200);

		let content = "";
		assert_eq!(extract_max_message_id_from_content(content), 0);

		// Test with sender info
		let content = "msg <!-- msg:300 user -->\nmsg <!-- msg:250 bot -->";
		assert_eq!(extract_max_message_id_from_content(content), 300);
	}
}
