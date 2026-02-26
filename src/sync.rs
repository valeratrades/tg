use std::{collections::BTreeMap, io::Write as _, path::Path};

use eyre::Result;
use grammers_client::Client;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::{
	config::{LiveSettings, TopicsMetadata},
	mtproto,
	pull::topic_filepath,
};

/// A message update to push to Telegram
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum MessageUpdate {
	Delete { group_id: u64, topic_id: u64, message_id: i32 },
	Edit { group_id: u64, topic_id: u64, message_id: i32, new_content: String },
	Create { group_id: u64, topic_id: u64, content: String },
}
/// Result of a single operation
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OpResult {
	pub success: bool,
	pub message: String,
}
/// Detailed results from a push operation
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PushResults {
	/// Results for delete operations: (group_id, message_id, result)
	pub deletions: Vec<(u64, i32, OpResult)>,
	/// Results for edit operations: (group_id, message_id, result)
	pub edits: Vec<(u64, i32, OpResult)>,
	/// Results for create operations: (group_id, topic_id, result with new message_id if successful)
	pub creates: Vec<(u64, u64, OpResult)>,
	/// Local file cleanup results: (file_path, lines_removed, message)
	pub file_cleanups: Vec<(String, usize, String)>,
}
/// Check server version and fail if mismatched
pub fn check_server_version(response: &crate::server::ServerResponse) -> Result<()> {
	let client_version = env!("CARGO_PKG_VERSION");
	match &response.version {
		Some(server_version) if server_version != client_version => {
			eyre::bail!(
				"Server version mismatch: server is v{server_version}, client is v{client_version}.\n\
				 Restart the server with `tg server` to use the updated version."
			);
		}
		None => {
			// Old server without version field - definitely outdated
			eyre::bail!(
				"Server is outdated (no version info). Client is v{client_version}.\n\
				 Restart the server with `tg server` to use the updated version."
			);
		}
		Some(_) => Ok(()), // Versions match
	}
}
/// Push updates via the running server (preferred) or fail with a clear error.
/// This avoids SQLite session file locking issues when the server is running.
/// Returns detailed results from the push operation.
pub async fn push_via_server(updates: Vec<MessageUpdate>, config: &LiveSettings) -> Result<PushResults> {
	use tokio::{
		io::{AsyncReadExt, AsyncWriteExt},
		net::TcpStream,
	};

	if updates.is_empty() {
		debug!("No updates to push");
		return Ok(PushResults::default());
	}

	let addr = format!("127.0.0.1:{}", config.config()?.localhost_port);

	let mut stream = match TcpStream::connect(&addr).await {
		Ok(s) => s,
		Err(e) => {
			eyre::bail!(
				"Cannot connect to server at {addr}: {e}\n\
				 Either the server is not running, or it's running on a different port.\n\
				 Start/restart it with `tg server`"
			);
		}
	};

	// Send push request
	let request = crate::server::ServerRequest::Push { updates };
	let request_json = serde_json::to_string(&request)?;
	stream.write_all(request_json.as_bytes()).await?;

	// Read response (may be larger than 4096 for many operations)
	let mut buf = vec![0u8; 65536];
	let n = stream.read(&mut buf).await?;
	if n == 0 {
		eyre::bail!("Server closed connection without response");
	}

	let response: crate::server::ServerResponse = serde_json::from_slice(&buf[..n])?;

	// Check version first
	check_server_version(&response)?;

	if response.success {
		Ok(response.push_results.unwrap_or_default())
	} else {
		eyre::bail!("Server push failed: {}", response.error.unwrap_or_else(|| "unknown error".to_string()))
	}
}
/// Push updates to Telegram and sync local files
/// - Deletes/edits messages on Telegram via MTProto
/// - Creates new messages via MTProto
/// - Removes deleted message lines from local topic files
///
/// NOTE: When called from the server, the client is already connected.
/// When called directly (CLI), use `push_via_server` instead to avoid database locking issues.
pub async fn push(updates: Vec<MessageUpdate>, _config: &LiveSettings, client: &Client) -> Result<PushResults> {
	let mut results = PushResults::default();

	if updates.is_empty() {
		debug!("No updates to push");
		return Ok(results);
	}

	let delete_count = updates.iter().filter(|u| matches!(u, MessageUpdate::Delete { .. })).count();
	let edit_count = updates.iter().filter(|u| matches!(u, MessageUpdate::Edit { .. })).count();
	let create_count = updates.iter().filter(|u| matches!(u, MessageUpdate::Create { .. })).count();
	let total = updates.len();

	info!("Pushing {total} updates ({delete_count} deletions, {edit_count} edits, {create_count} creates)");
	eprintln!("Pushing {total} updates ({delete_count} deletions, {edit_count} edits, {create_count} creates)");

	// Sanity check for many changes
	if total > 25 {
		eprint!("About to modify {total} messages on Telegram. Continue? [y/N] ");
		std::io::stdout().flush()?;
		let mut input = String::new();
		std::io::stdin().read_line(&mut input)?;
		if !input.trim().eq_ignore_ascii_case("y") {
			info!("Aborted by user");
			eprintln!("Aborted.");
			return Ok(results);
		}
	}

	// Group deletions by group_id for batch delete
	let mut deletions_by_group: BTreeMap<u64, Vec<(u64, i32)>> = BTreeMap::new(); // group_id -> [(topic_id, msg_id)]
	let mut edits: Vec<(u64, u64, i32, String)> = Vec::new(); // (group_id, topic_id, msg_id, new_content)
	let mut creates: Vec<(u64, u64, String)> = Vec::new(); // (group_id, topic_id, content)

	for update in &updates {
		match update {
			MessageUpdate::Delete { group_id, topic_id, message_id } => {
				deletions_by_group.entry(*group_id).or_default().push((*topic_id, *message_id));
			}
			MessageUpdate::Edit {
				group_id,
				topic_id,
				message_id,
				new_content,
			} => {
				edits.push((*group_id, *topic_id, *message_id, new_content.clone()));
			}
			MessageUpdate::Create { group_id, topic_id, content } => {
				creates.push((*group_id, *topic_id, content.clone()));
			}
		}
	}

	// Apply deletions (batch per group), track successful deletions
	let mut successful_deletions: BTreeMap<u64, Vec<(u64, i32)>> = BTreeMap::new();

	for (group_id, items) in &deletions_by_group {
		let msg_ids: Vec<i32> = items.iter().map(|(_, id)| *id).collect();
		match mtproto::delete_messages(client, *group_id, &msg_ids).await {
			Ok(count) => {
				info!(group_id, count, "Deleted messages from Telegram");
				if count > 0 {
					successful_deletions.insert(*group_id, items.clone());
					for (_, msg_id) in items {
						results.deletions.push((
							*group_id,
							*msg_id,
							OpResult {
								success: true,
								message: format!("Deleted (pts_count={count})"),
							},
						));
					}
				} else {
					warn!(group_id, "Telegram reported 0 deletions, keeping local files intact");
					for (_, msg_id) in items {
						results.deletions.push((
							*group_id,
							*msg_id,
							OpResult {
								success: false,
								message: "Telegram returned 0 deletions".to_string(),
							},
						));
					}
				}
			}
			Err(e) => {
				warn!(group_id, error = %e, "Failed to delete messages");
				for (_, msg_id) in items {
					results.deletions.push((
						*group_id,
						*msg_id,
						OpResult {
							success: false,
							message: e.to_string(),
						},
					));
				}
			}
		}
	}

	// Apply edits - all via MTProto
	for (group_id, _topic_id, msg_id, new_content) in &edits {
		match mtproto::edit_message(client, *group_id, *msg_id, new_content).await {
			Ok(()) => {
				info!(group_id, msg_id, "Edited message on Telegram");
				results.edits.push((
					*group_id,
					*msg_id,
					OpResult {
						success: true,
						message: "Edited via MTProto".to_string(),
					},
				));
			}
			Err(e) => {
				warn!(group_id, msg_id, error = %e, "Failed to edit message");
				results.edits.push((
					*group_id,
					*msg_id,
					OpResult {
						success: false,
						message: e.to_string(),
					},
				));
			}
		}
	}

	// Apply creates - send new messages via MTProto
	let mut successful_creates: Vec<(u64, u64, String, i32)> = Vec::new(); // (group_id, topic_id, content, msg_id)

	for (group_id, topic_id, content) in &creates {
		match mtproto::send_text_message(client, *group_id, *topic_id, content).await {
			Ok(msg_id) => {
				info!(group_id, topic_id, msg_id, "Created message on Telegram");
				successful_creates.push((*group_id, *topic_id, content.clone(), msg_id));
				results.creates.push((
					*group_id,
					*topic_id,
					OpResult {
						success: true,
						message: format!("Created with msg_id={msg_id}"),
					},
				));
			}
			Err(e) => {
				warn!(group_id, topic_id, error = %e, "Failed to create message");
				results.creates.push((
					*group_id,
					*topic_id,
					OpResult {
						success: false,
						message: e.to_string(),
					},
				));
			}
		}
	}

	// Remove deleted messages from local topic files (only for successful deletions)
	if successful_deletions.is_empty() && delete_count > 0 {
		info!("No successful deletions, skipping local file cleanup");
		eprintln!("No local files modified (Telegram deletions failed or returned 0)");
	}

	let metadata = TopicsMetadata::load();
	let msg_id_re = Regex::new(r"<!-- (?:forwarded )?msg:(\d+)(?: ts:(\d+))?(?: reply_to:(\d+))?((?:\s+\w+)*) -->").unwrap();

	for (group_id, items) in &successful_deletions {
		// Group by topic_id
		let mut by_topic: BTreeMap<u64, Vec<i32>> = BTreeMap::new();
		for (topic_id, msg_id) in items {
			by_topic.entry(*topic_id).or_default().push(*msg_id);
		}

		for (topic_id, msg_ids) in by_topic {
			let file_path = topic_filepath(*group_id, topic_id, &metadata);
			debug!(path = %file_path.display(), "Looking for topic file to clean up");
			if !file_path.exists() {
				warn!(path = %file_path.display(), "Topic file does not exist, skipping cleanup");
				results.file_cleanups.push((file_path.display().to_string(), 0, "File does not exist".to_string()));
				continue;
			}

			let content = match std::fs::read_to_string(&file_path) {
				Ok(c) => c,
				Err(e) => {
					warn!(path = %file_path.display(), error = %e, "Failed to read topic file");
					results.file_cleanups.push((file_path.display().to_string(), 0, format!("Failed to read: {e}")));
					continue;
				}
			};

			let msg_id_set: std::collections::HashSet<i32> = msg_ids.into_iter().collect();
			debug!(?msg_id_set, "Looking for message IDs to remove");
			let mut removed = 0;

			// Filter out lines with deleted message IDs
			let new_lines: Vec<&str> = content
				.lines()
				.filter(|line| {
					if let Some(caps) = msg_id_re.captures(line)
						&& let Ok(id) = caps.get(1).unwrap().as_str().parse::<i32>()
						&& msg_id_set.contains(&id)
					{
						debug!(line, id, "Removing line with deleted message ID");
						removed += 1;
						return false;
					}
					true
				})
				.collect();

			if removed > 0 {
				let new_content = new_lines.join("\n");
				if let Err(e) = std::fs::write(&file_path, new_content) {
					warn!(path = %file_path.display(), error = %e, "Failed to write topic file");
					results.file_cleanups.push((file_path.display().to_string(), 0, format!("Failed to write: {e}")));
				} else {
					info!(path = %file_path.display(), removed, "Removed lines from topic file");
					results.file_cleanups.push((file_path.display().to_string(), removed, format!("Removed {removed} line(s)")));
				}
			} else {
				warn!(path = %file_path.display(), ?msg_id_set, "No lines removed - message IDs not found in file");
				results
					.file_cleanups
					.push((file_path.display().to_string(), 0, format!("Message IDs {msg_id_set:?} not found in file")));
			}
		}
	}

	// Update local files for successful creates: add message ID tags
	// We group by topic to batch updates
	if !successful_creates.is_empty() {
		use jiff::Timestamp;

		use crate::server::format_message_append_with_sender;

		let mut creates_by_topic: BTreeMap<(u64, u64), Vec<(String, i32)>> = BTreeMap::new();
		for (group_id, topic_id, content, msg_id) in successful_creates {
			creates_by_topic.entry((group_id, topic_id)).or_default().push((content, msg_id));
		}

		for ((group_id, topic_id), messages) in creates_by_topic {
			let file_path = topic_filepath(group_id, topic_id, &metadata);

			// Read current file content
			let mut file_content = std::fs::read_to_string(&file_path).unwrap_or_default();

			// Remove untagged lines from the end (the ones we just sent)
			// We need to be careful to only remove the content we sent
			for (content, _) in &messages {
				// Try to find and remove the untagged content from the file
				// The content might have ". " prefix stripped, so check both forms
				let patterns_to_remove = [format!("\n. {content}"), format!("\n{content}"), content.clone()];

				for pattern in &patterns_to_remove {
					if let Some(pos) = file_content.rfind(pattern) {
						// Verify this is untagged (no <!-- msg: after it on the same logical block)
						let after = &file_content[pos + pattern.len()..];
						let next_newline = after.find('\n').unwrap_or(after.len());
						let rest_of_line = &after[..next_newline];
						if !rest_of_line.contains("<!-- msg:") {
							// Remove this content
							file_content = format!("{}{}", &file_content[..pos], &file_content[pos + pattern.len()..]);
							break;
						}
					}
				}
			}

			// Now append the messages with proper tags
			// Get the last write time from existing file content to format correctly
			let now = Timestamp::now();
			for (content, msg_id) in messages {
				let formatted = format_message_append_with_sender(&content, None, now, Some(msg_id), None);
				file_content.push_str(&formatted);
			}

			// Write back
			if let Err(e) = std::fs::write(&file_path, &file_content) {
				warn!(path = %file_path.display(), error = %e, "Failed to update topic file with message IDs");
			} else {
				info!(path = %file_path.display(), "Updated topic file with message IDs");
			}
		}
	}

	info!("Push complete");
	Ok(results)
}
/// Parsed message with content
#[derive(Clone, Debug)]
pub struct ParsedMessage {
	pub content: String,
	pub is_voice: bool,
	pub reply_to_msg_id: Option<i32>,
	/// UTC unix timestamp from the message tag (None for old messages without ts:)
	pub ts: Option<i64>,
}
/// Parse a topic file and extract all messages with their IDs
/// Returns a map of message_id -> ParsedMessage
pub fn parse_file_messages(content: &str) -> BTreeMap<i32, ParsedMessage> {
	let mut messages = BTreeMap::new();
	let msg_id_re = Regex::new(r"<!-- (?:forwarded )?msg:(\d+)(?: ts:(\d+))?(?: reply_to:(\d+))?((?:\s+\w+)*) -->").unwrap();

	let lines: Vec<&str> = content.lines().collect();
	let mut i = 0;

	while i < lines.len() {
		let line = lines[i];
		let trimmed = line.trim();

		// Check if this is a code block start (5+ backticks with md)
		if trimmed.starts_with("`````") && trimmed.contains("md") {
			// Collect code block content until closing fence
			let mut block_content = Vec::new();
			i += 1;

			while i < lines.len() {
				let next_line = lines[i];
				let next_trimmed = next_line.trim();

				// Check for closing fence (pure backticks, 5+)
				if next_trimmed.chars().all(|c| c == '`') && next_trimmed.len() >= 5 {
					// New format: tag on next line
					i += 1;
					if i < lines.len()
						&& let Some(caps) = msg_id_re.captures(lines[i])
						&& let Ok(id) = caps.get(1).unwrap().as_str().parse::<i32>()
					{
						let is_voice = has_voice_qualifier(&caps);
						let reply_to_msg_id = extract_reply_to(&caps);
						let ts = extract_ts(&caps);
						let msg_content = block_content.join("\n");
						messages.insert(
							id,
							ParsedMessage {
								content: msg_content,
								is_voice,
								reply_to_msg_id,
								ts,
							},
						);
					}
					i += 1;
					break;
				} else if next_trimmed.contains("`````") {
					// Legacy format: closing fence has tag on same line
					if let Some(caps) = msg_id_re.captures(next_line)
						&& let Ok(id) = caps.get(1).unwrap().as_str().parse::<i32>()
					{
						let is_voice = has_voice_qualifier(&caps);
						let reply_to_msg_id = extract_reply_to(&caps);
						let ts = extract_ts(&caps);
						let msg_content = block_content.join("\n");
						messages.insert(
							id,
							ParsedMessage {
								content: msg_content,
								is_voice,
								reply_to_msg_id,
								ts,
							},
						);
					}
					i += 1;
					break;
				}

				block_content.push(next_line);
				i += 1;
			}
			continue;
		}

		// Simple single-line message with tag on same line
		if let Some(caps) = msg_id_re.captures(line)
			&& let Ok(id) = caps.get(1).unwrap().as_str().parse::<i32>()
		{
			let is_voice = has_voice_qualifier(&caps);
			let reply_to_msg_id = extract_reply_to(&caps);
			let ts = extract_ts(&caps);
			// Extract content by removing the message ID marker
			let msg_content = msg_id_re.replace(line, "").trim().to_string();
			messages.insert(
				id,
				ParsedMessage {
					content: msg_content,
					is_voice,
					reply_to_msg_id,
					ts,
				},
			);
		}

		i += 1;
	}

	messages
}
/// Information about file content structure for detecting new messages
#[derive(Debug)]
pub struct FileContentInfo {
	/// Line number (0-indexed) of the last tagged message, if any
	pub last_tagged_line: Option<usize>,
	/// All tagged messages with their line numbers
	pub tagged_messages: BTreeMap<i32, ParsedMessage>,
	/// Lines that are not tagged messages (line_number, content)
	/// Includes date headers (## MMM DD or ## MMM DD, YYYY) and message prefixes (. )
	pub untagged_lines: Vec<(usize, String)>,
}
/// Parse file content and track line positions for new message detection
pub fn parse_file_with_positions(content: &str) -> FileContentInfo {
	let msg_id_re = Regex::new(r"<!-- (?:forwarded )?msg:(\d+)(?: ts:(\d+))?(?: reply_to:(\d+))?((?:\s+\w+)*) -->").unwrap();

	let mut info = FileContentInfo {
		last_tagged_line: None,
		tagged_messages: BTreeMap::new(),
		untagged_lines: Vec::new(),
	};

	for (line_num, line) in content.lines().enumerate() {
		if let Some(caps) = msg_id_re.captures(line) {
			if let Ok(id) = caps.get(1).unwrap().as_str().parse::<i32>() {
				let is_voice = has_voice_qualifier(&caps);
				let reply_to_msg_id = extract_reply_to(&caps);
				let ts = extract_ts(&caps);
				let msg_content = msg_id_re.replace(line, "").trim().to_string();
				info.tagged_messages.insert(
					id,
					ParsedMessage {
						content: msg_content,
						is_voice,
						reply_to_msg_id,
						ts,
					},
				);
				info.last_tagged_line = Some(line_num);
			}
		} else {
			info.untagged_lines.push((line_num, line.to_string()));
		}
	}

	info
}
/// Detect changes between old and new file content, including new messages to send
pub fn detect_changes_with_new_messages(old_content: &str, new_content: &str) -> FileChanges {
	let old_info = parse_file_with_positions(old_content);
	let new_info = parse_file_with_positions(new_content);

	// Start with standard edit/delete detection
	let mut changes = detect_changes(&old_info.tagged_messages, &new_info.tagged_messages);

	// Now detect new messages: untagged content in new file that wasn't in old file
	// We need to figure out what content was added after the last known message

	// Find the last tagged line in the NEW file to determine the boundary
	let last_tagged_line = new_info.last_tagged_line;

	// Collect untagged lines that appear AFTER the last tagged message
	// These are potential new messages to send
	let mut new_content_after_last: Vec<String> = Vec::new();
	let mut invalid_content_before_last: Vec<String> = Vec::new();

	// Build a set of old untagged lines for comparison
	let old_untagged_set: std::collections::HashSet<String> = old_info.untagged_lines.iter().map(|(_, s)| s.clone()).collect();

	for (line_num, line_content) in &new_info.untagged_lines {
		// Skip empty lines, date headers, and whitespace-only lines
		let trimmed = line_content.trim();
		if trimmed.is_empty() {
			continue;
		}
		// Skip date headers (## MMM DD or ## MMM DD, YYYY)
		if trimmed.starts_with("## ") && trimmed.len() <= 16 {
			continue;
		}

		// Check if this line existed in the old file
		if old_untagged_set.contains(line_content) {
			continue;
		}

		// This is new content - check if it's after or before the last tagged message
		match last_tagged_line {
			Some(last_line) if *line_num <= last_line => {
				// Content was inserted before the last known message
				invalid_content_before_last.push(line_content.clone());
			}
			_ => {
				// Content is after the last known message (or there are no tagged messages)
				new_content_after_last.push(line_content.clone());
			}
		}
	}

	// Combine consecutive lines into messages
	// Lines starting with ". " are message separators, so each such line starts a new message
	// Other consecutive lines belong together
	changes.created = coalesce_new_messages(&new_content_after_last);
	changes.invalid_inserts = invalid_content_before_last;

	changes
}
/// Convert FileChanges to MessageUpdates for a given group/topic
pub fn changes_to_updates(changes: &FileChanges, group_id: u64, topic_id: u64) -> Vec<MessageUpdate> {
	let mut updates = Vec::new();

	for msg_id in &changes.deleted {
		updates.push(MessageUpdate::Delete {
			group_id,
			topic_id,
			message_id: *msg_id,
		});
	}

	for (msg_id, new_content) in &changes.edited {
		updates.push(MessageUpdate::Edit {
			group_id,
			topic_id,
			message_id: *msg_id,
			new_content: new_content.clone(),
		});
	}

	for content in &changes.created {
		updates.push(MessageUpdate::Create {
			group_id,
			topic_id,
			content: content.clone(),
		});
	}

	updates
}
/// Represents changes detected between old and new file states
#[derive(Clone, Debug, Default)]
pub struct FileChanges {
	/// Messages that were deleted (message_id)
	pub deleted: Vec<i32>,
	/// Messages that were edited (message_id, new_content)
	pub edited: Vec<(i32, String)>,
	/// New messages to be created (content only)
	pub created: Vec<String>,
	/// Content that was added before the last known message (cannot be sent back in time)
	pub invalid_inserts: Vec<String>,
}
impl FileChanges {
	pub fn is_empty(&self) -> bool {
		self.deleted.is_empty() && self.edited.is_empty() && self.created.is_empty()
	}

	pub fn has_invalid_inserts(&self) -> bool {
		!self.invalid_inserts.is_empty()
	}
}

/// Compare old and new file states to detect changes
pub fn detect_changes(old_state: &BTreeMap<i32, ParsedMessage>, new_state: &BTreeMap<i32, ParsedMessage>) -> FileChanges {
	let mut changes = FileChanges::default();

	// Find deleted messages (in old but not in new)
	for id in old_state.keys() {
		if !new_state.contains_key(id) {
			changes.deleted.push(*id);
		}
	}

	// Find edited messages (in both but content differs)
	for (id, new_msg) in new_state {
		if let Some(old_msg) = old_state.get(id)
			&& old_msg.content != new_msg.content
			&& !old_msg.is_voice
			&& !new_msg.is_voice
		{
			changes.edited.push((*id, new_msg.content.clone()));
		}
	}

	changes
}
/// Resolve a topic file path to (group_id, topic_id)
pub fn resolve_topic_ids_from_path(path: &Path) -> Option<(u64, u64)> {
	let metadata = TopicsMetadata::load();
	let data_dir = crate::server::DATA_DIR.get()?;

	// Extract relative path from data dir
	let rel_path = path.strip_prefix(data_dir).ok()?;

	// Expected format: group_name/topic_name.md
	let mut components = rel_path.components();
	let group_name = components.next()?.as_os_str().to_str()?;
	let topic_file = components.next()?.as_os_str().to_str()?;
	let topic_name = topic_file.strip_suffix(".md")?;

	// Find group_id by matching group name
	for (group_id, group) in &metadata.groups {
		let default_name = format!("group_{group_id}");
		let gname = group.name.as_deref().unwrap_or(&default_name);
		if gname == group_name {
			// Find topic_id by matching topic name
			for (topic_id, tname) in &group.topics {
				if tname == topic_name {
					return Some((*group_id, *topic_id));
				}
			}
		}
	}

	None
}
fn has_voice_qualifier(caps: &regex::Captures<'_>) -> bool {
	caps.get(4).map(|m| m.as_str().split_whitespace().any(|w| w == "voice")).unwrap_or(false)
}
fn extract_reply_to(caps: &regex::Captures<'_>) -> Option<i32> {
	caps.get(3).and_then(|m| m.as_str().parse().ok())
}
pub(crate) fn extract_ts(caps: &regex::Captures<'_>) -> Option<i64> {
	caps.get(2).and_then(|m| m.as_str().parse().ok())
}

/// Combine lines into discrete messages
/// Lines starting with ". " mark message boundaries
fn coalesce_new_messages(lines: &[String]) -> Vec<String> {
	if lines.is_empty() {
		return Vec::new();
	}

	let mut messages = Vec::new();
	let mut current_message = String::new();

	for line in lines {
		let trimmed = line.trim();

		// Skip empty lines between messages
		if trimmed.is_empty() {
			continue;
		}

		// ". " prefix indicates start of a new message block
		if let Some(stripped) = trimmed.strip_prefix(". ") {
			// Save current message if non-empty
			if !current_message.is_empty() {
				messages.push(current_message.trim().to_string());
			}
			// Start new message without the ". " prefix
			current_message = stripped.to_string();
		} else if current_message.is_empty() {
			// First line of a new message
			current_message = trimmed.to_string();
		} else {
			// Continuation of current message
			current_message.push('\n');
			current_message.push_str(trimmed);
		}
	}

	// Don't forget the last message
	if !current_message.is_empty() {
		messages.push(current_message.trim().to_string());
	}

	messages
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_check_server_version_match() {
		let response = crate::server::ServerResponse {
			success: true,
			error: None,
			version: Some(env!("CARGO_PKG_VERSION").to_string()),
			push_results: None,
		};
		assert!(check_server_version(&response).is_ok());
	}

	#[test]
	fn test_check_server_version_mismatch() {
		let response = crate::server::ServerResponse {
			success: true,
			error: None,
			version: Some("0.0.1".to_string()), // old version
			push_results: None,
		};
		let err = check_server_version(&response).unwrap_err();
		assert!(err.to_string().contains("version mismatch"));
		assert!(err.to_string().contains("Restart the server"));
	}

	#[test]
	fn test_check_server_version_missing() {
		let response = crate::server::ServerResponse {
			success: true,
			error: None,
			version: None, // very old server without version field
			push_results: None,
		};
		let err = check_server_version(&response).unwrap_err();
		assert!(err.to_string().contains("outdated"));
		assert!(err.to_string().contains("Restart the server"));
	}

	#[test]
	fn test_parse_file_messages() {
		let content = r#"Hello world <!-- msg:123 -->

## Jan 03
This is a test <!-- msg:456 bot -->

. Another message <!-- msg:789 user -->
"#;

		let messages = parse_file_messages(content);
		assert_eq!(messages.len(), 3);
		assert_eq!(messages.get(&123).map(|m| &m.content), Some(&"Hello world".to_string()));
		assert_eq!(messages.get(&456).map(|m| &m.content), Some(&"This is a test".to_string()));
		assert_eq!(messages.get(&789).map(|m| &m.content), Some(&". Another message".to_string()));
	}

	#[test]
	fn test_parse_file_messages_new_codeblock_format() {
		// New format: closing fence on its own line, tag on next line
		let content = r#"`````md
This is a multi-line message

With paragraph breaks
`````
<!-- msg:123 bot -->

simple message <!-- msg:456 user -->
"#;

		let messages = parse_file_messages(content);
		assert_eq!(messages.len(), 2);
		assert_eq!(messages.get(&123).map(|m| &m.content), Some(&"This is a multi-line message\n\nWith paragraph breaks".to_string()));
		assert_eq!(messages.get(&456).map(|m| &m.content), Some(&"simple message".to_string()));
	}

	#[test]
	fn test_parse_file_messages_legacy_codeblock_format() {
		// Legacy format: closing fence has tag on same line
		let content = r#"`````md
Legacy multi-line message
````` <!-- msg:789 bot -->
"#;

		let messages = parse_file_messages(content);
		assert_eq!(messages.len(), 1);
		assert_eq!(messages.get(&789).map(|m| &m.content), Some(&"Legacy multi-line message".to_string()));
	}

	#[test]
	fn test_detect_changes_deleted() {
		let mut old = BTreeMap::new();
		old.insert(
			1,
			ParsedMessage {
				content: "message 1".to_string(),
				is_voice: false,
				reply_to_msg_id: None,
				ts: None,
			},
		);
		old.insert(
			2,
			ParsedMessage {
				content: "message 2".to_string(),
				is_voice: false,
				reply_to_msg_id: None,
				ts: None,
			},
		);

		let mut new = BTreeMap::new();
		new.insert(
			1,
			ParsedMessage {
				content: "message 1".to_string(),
				is_voice: false,
				reply_to_msg_id: None,
				ts: None,
			},
		);
		// message 2 is deleted

		let changes = detect_changes(&old, &new);
		assert_eq!(changes.deleted, vec![2]);
		assert!(changes.edited.is_empty());
	}

	#[test]
	fn test_detect_changes_edited() {
		let mut old = BTreeMap::new();
		old.insert(
			1,
			ParsedMessage {
				content: "message 1".to_string(),
				is_voice: false,
				reply_to_msg_id: None,
				ts: None,
			},
		);
		old.insert(
			2,
			ParsedMessage {
				content: "message 2".to_string(),
				is_voice: false,
				reply_to_msg_id: None,
				ts: None,
			},
		);

		let mut new = BTreeMap::new();
		new.insert(
			1,
			ParsedMessage {
				content: "message 1".to_string(),
				is_voice: false,
				reply_to_msg_id: None,
				ts: None,
			},
		);
		new.insert(
			2,
			ParsedMessage {
				content: "message 2 edited".to_string(),
				is_voice: false,
				reply_to_msg_id: None,
				ts: None,
			},
		);

		let changes = detect_changes(&old, &new);
		assert!(changes.deleted.is_empty());
		assert_eq!(changes.edited, vec![(2, "message 2 edited".to_string())]);
	}

	#[test]
	fn test_detect_changes_mixed() {
		let mut old = BTreeMap::new();
		old.insert(
			1,
			ParsedMessage {
				content: "message 1".to_string(),
				is_voice: false,
				reply_to_msg_id: None,
				ts: None,
			},
		);
		old.insert(
			2,
			ParsedMessage {
				content: "message 2".to_string(),
				is_voice: false,
				reply_to_msg_id: None,
				ts: None,
			},
		);
		old.insert(
			3,
			ParsedMessage {
				content: "message 3".to_string(),
				is_voice: false,
				reply_to_msg_id: None,
				ts: None,
			},
		);

		let mut new = BTreeMap::new();
		new.insert(
			1,
			ParsedMessage {
				content: "message 1 edited".to_string(),
				is_voice: false,
				reply_to_msg_id: None,
				ts: None,
			},
		);
		// message 2 deleted
		new.insert(
			3,
			ParsedMessage {
				content: "message 3".to_string(),
				is_voice: false,
				reply_to_msg_id: None,
				ts: None,
			},
		);

		let changes = detect_changes(&old, &new);
		assert_eq!(changes.deleted, vec![2]);
		assert_eq!(changes.edited, vec![(1, "message 1 edited".to_string())]);
	}

	#[test]
	fn test_detect_new_messages_at_end() {
		let old_content = r#"Hello world <!-- msg:123 -->

## Jan 03
This is a test <!-- msg:456 bot -->
"#;

		let new_content = r#"Hello world <!-- msg:123 -->

## Jan 03
This is a test <!-- msg:456 bot -->

New message at the end
"#;

		let changes = detect_changes_with_new_messages(old_content, new_content);
		assert!(changes.deleted.is_empty());
		assert!(changes.edited.is_empty());
		assert_eq!(changes.created.len(), 1);
		assert_eq!(changes.created[0], "New message at the end");
		assert!(changes.invalid_inserts.is_empty());
	}

	#[test]
	fn test_detect_new_messages_with_dot_prefix() {
		let old_content = r#"Hello world <!-- msg:123 -->
"#;

		let new_content = r#"Hello world <!-- msg:123 -->

. First new message

. Second new message
"#;

		let changes = detect_changes_with_new_messages(old_content, new_content);
		assert!(changes.deleted.is_empty());
		assert!(changes.edited.is_empty());
		assert_eq!(changes.created.len(), 2);
		assert_eq!(changes.created[0], "First new message");
		assert_eq!(changes.created[1], "Second new message");
		assert!(changes.invalid_inserts.is_empty());
	}

	#[test]
	fn test_detect_invalid_insert_before_last_message() {
		let old_content = r#"Hello world <!-- msg:123 -->

## Jan 03
This is a test <!-- msg:456 bot -->
"#;

		let new_content = r#"Hello world <!-- msg:123 -->

Inserted in the middle

## Jan 03
This is a test <!-- msg:456 bot -->
"#;

		let changes = detect_changes_with_new_messages(old_content, new_content);
		assert!(changes.deleted.is_empty());
		assert!(changes.edited.is_empty());
		assert!(changes.created.is_empty());
		assert_eq!(changes.invalid_inserts.len(), 1);
		assert!(changes.invalid_inserts[0].contains("Inserted in the middle"));
	}

	#[test]
	fn test_detect_new_messages_empty_file() {
		let old_content = "";
		let new_content = "New message in empty file";

		let changes = detect_changes_with_new_messages(old_content, new_content);
		assert!(changes.deleted.is_empty());
		assert!(changes.edited.is_empty());
		assert_eq!(changes.created.len(), 1);
		assert_eq!(changes.created[0], "New message in empty file");
		assert!(changes.invalid_inserts.is_empty());
	}

	#[test]
	fn test_detect_multiline_new_message() {
		let old_content = r#"Hello world <!-- msg:123 -->
"#;

		let new_content = r#"Hello world <!-- msg:123 -->

This is a multiline message
with multiple lines
that should be combined
"#;

		let changes = detect_changes_with_new_messages(old_content, new_content);
		assert!(changes.deleted.is_empty());
		assert!(changes.edited.is_empty());
		assert_eq!(changes.created.len(), 1);
		assert!(changes.created[0].contains("multiline message"));
		assert!(changes.created[0].contains("multiple lines"));
		assert!(changes.invalid_inserts.is_empty());
	}

	#[test]
	fn parse_file_messages_reply_to() {
		let content = "check this out <!-- msg:100 reply_to:99 user -->\noriginal message <!-- msg:99 user -->\n";
		let messages = parse_file_messages(content);
		insta::assert_debug_snapshot!(messages, @r#"
		{
		    99: ParsedMessage {
		        content: "original message",
		        is_voice: false,
		        reply_to_msg_id: None,
		        ts: None,
		    },
		    100: ParsedMessage {
		        content: "check this out",
		        is_voice: false,
		        reply_to_msg_id: Some(
		            99,
		        ),
		        ts: None,
		    },
		}
		"#);
	}

	#[test]
	fn test_coalesce_new_messages() {
		let lines = vec![
			"First message".to_string(),
			"continuation of first".to_string(),
			". Second message".to_string(),
			". Third message".to_string(),
		];

		let messages = coalesce_new_messages(&lines);
		assert_eq!(messages.len(), 3);
		assert!(messages[0].contains("First message"));
		assert!(messages[0].contains("continuation of first"));
		assert_eq!(messages[1], "Second message");
		assert_eq!(messages[2], "Third message");
	}
}
