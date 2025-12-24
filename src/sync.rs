use std::{collections::BTreeMap, io::Write as _, path::Path};

use eyre::Result;
use regex::Regex;
use tracing::{debug, info, warn};

use crate::{
	config::{AppConfig, TopicsMetadata},
	mtproto,
	pull::topic_filepath,
};

/// A message update to push to Telegram
#[derive(Clone, Debug)]
pub enum MessageUpdate {
	Delete { group_id: u64, topic_id: u64, message_id: i32 },
	Edit { group_id: u64, message_id: i32, new_content: String },
}

/// Push updates to Telegram and sync local files
/// - Deletes/edits messages on Telegram via MTProto
/// - Removes deleted message lines from local topic files
pub async fn push(updates: Vec<MessageUpdate>, config: &AppConfig) -> Result<()> {
	if updates.is_empty() {
		debug!("No updates to push");
		return Ok(());
	}

	let delete_count = updates.iter().filter(|u| matches!(u, MessageUpdate::Delete { .. })).count();
	let edit_count = updates.iter().filter(|u| matches!(u, MessageUpdate::Edit { .. })).count();
	let total = updates.len();

	info!("Pushing {} updates ({} deletions, {} edits)", total, delete_count, edit_count);
	eprintln!("Pushing {} updates ({} deletions, {} edits)", total, delete_count, edit_count);

	// Sanity check for many changes
	if total > 25 {
		eprint!("About to modify {} messages on Telegram. Continue? [y/N] ", total);
		std::io::stdout().flush()?;
		let mut input = String::new();
		std::io::stdin().read_line(&mut input)?;
		if !input.trim().eq_ignore_ascii_case("y") {
			info!("Aborted by user");
			eprintln!("Aborted.");
			return Ok(());
		}
	}

	// Create MTProto client
	let (client, handle) = mtproto::create_client(config).await?;

	// Group deletions by group_id for batch delete
	let mut deletions_by_group: BTreeMap<u64, Vec<(u64, i32)>> = BTreeMap::new(); // group_id -> [(topic_id, msg_id)]
	let mut edits: Vec<(u64, i32, String)> = Vec::new(); // (group_id, msg_id, new_content)

	for update in &updates {
		match update {
			MessageUpdate::Delete { group_id, topic_id, message_id } => {
				deletions_by_group.entry(*group_id).or_default().push((*topic_id, *message_id));
			}
			MessageUpdate::Edit { group_id, message_id, new_content } => {
				edits.push((*group_id, *message_id, new_content.clone()));
			}
		}
	}

	// Apply deletions (batch per group), track successful deletions
	let mut successful_deletions: BTreeMap<u64, Vec<(u64, i32)>> = BTreeMap::new();

	for (group_id, items) in &deletions_by_group {
		let msg_ids: Vec<i32> = items.iter().map(|(_, id)| *id).collect();
		match mtproto::delete_messages(&client, *group_id, &msg_ids).await {
			Ok(count) => {
				info!(group_id, count, "Deleted messages from Telegram");
				eprintln!("Deleted {} message(s) from group {}", count, group_id);
				// Only track as successful if at least some messages were deleted
				// Note: Telegram returns pts_count, not individual success per message
				// If count matches requested, all succeeded; if count > 0 but < requested,
				// we can't know which ones failed, so conservatively mark all as successful
				// (Telegram usually deletes all or none)
				if count > 0 {
					successful_deletions.insert(*group_id, items.clone());
				} else {
					warn!(group_id, "Telegram reported 0 deletions, keeping local files intact");
					eprintln!("Warning: No messages were deleted from Telegram, local files unchanged");
				}
			}
			Err(e) => {
				warn!(group_id, error = %e, "Failed to delete messages");
				eprintln!("Failed to delete from group {}: {}", group_id, e);
				// Don't add to successful_deletions - local files stay intact
			}
		}
	}

	// Apply edits
	for (group_id, msg_id, new_content) in &edits {
		match mtproto::edit_message(&client, *group_id, *msg_id, new_content).await {
			Ok(()) => {
				info!(group_id, msg_id, "Edited message on Telegram");
				eprintln!("Edited message {} in group {}", msg_id, group_id);
			}
			Err(e) => {
				warn!(group_id, msg_id, error = %e, "Failed to edit message");
				eprintln!("Failed to edit message {} in group {}: {}", msg_id, group_id, e);
			}
		}
	}

	client.disconnect();
	handle.abort();

	// Remove deleted messages from local topic files (only for successful deletions)
	if successful_deletions.is_empty() && !deletions_by_group.is_empty() {
		info!("No successful deletions, skipping local file cleanup");
		eprintln!("No local files modified (Telegram deletions failed or returned 0)");
	}

	let metadata = TopicsMetadata::load();
	let msg_id_re = Regex::new(r"<!-- msg:(\d+) -->").unwrap();

	for (group_id, items) in &successful_deletions {
		// Group by topic_id
		let mut by_topic: BTreeMap<u64, Vec<i32>> = BTreeMap::new();
		for (topic_id, msg_id) in items {
			by_topic.entry(*topic_id).or_default().push(*msg_id);
		}

		for (topic_id, msg_ids) in by_topic {
			let file_path = topic_filepath(*group_id, topic_id, &metadata);
			if !file_path.exists() {
				continue;
			}

			let content = match std::fs::read_to_string(&file_path) {
				Ok(c) => c,
				Err(e) => {
					warn!(path = %file_path.display(), error = %e, "Failed to read topic file");
					continue;
				}
			};

			let msg_id_set: std::collections::HashSet<i32> = msg_ids.into_iter().collect();
			let mut removed = 0;

			// Filter out lines with deleted message IDs
			let new_lines: Vec<&str> = content
				.lines()
				.filter(|line| {
					if let Some(caps) = msg_id_re.captures(line) {
						if let Ok(id) = caps.get(1).unwrap().as_str().parse::<i32>() {
							if msg_id_set.contains(&id) {
								removed += 1;
								return false;
							}
						}
					}
					true
				})
				.collect();

			if removed > 0 {
				let new_content = new_lines.join("\n");
				if let Err(e) = std::fs::write(&file_path, new_content) {
					warn!(path = %file_path.display(), error = %e, "Failed to write topic file");
				} else {
					info!(path = %file_path.display(), removed, "Removed lines from topic file");
					eprintln!("Removed {} line(s) from {}", removed, file_path.display());
				}
			}
		}
	}

	info!("Push complete");
	Ok(())
}

/// Parse a topic file and extract all messages with their IDs
/// Returns a map of message_id -> content
pub fn parse_file_messages(content: &str) -> BTreeMap<i32, String> {
	let mut messages = BTreeMap::new();
	let msg_id_re = Regex::new(r"<!-- msg:(\d+) -->").unwrap();

	for line in content.lines() {
		if let Some(caps) = msg_id_re.captures(line) {
			if let Ok(id) = caps.get(1).unwrap().as_str().parse::<i32>() {
				// Extract content by removing the message ID marker
				let content = msg_id_re.replace(line, "").trim().to_string();
				messages.insert(id, content);
			}
		}
	}

	messages
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
			message_id: *msg_id,
			new_content: new_content.clone(),
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
}

impl FileChanges {
	pub fn is_empty(&self) -> bool {
		self.deleted.is_empty() && self.edited.is_empty()
	}
}

/// Compare old and new file states to detect changes
pub fn detect_changes(old_state: &BTreeMap<i32, String>, new_state: &BTreeMap<i32, String>) -> FileChanges {
	let mut changes = FileChanges::default();

	// Find deleted messages (in old but not in new)
	for (id, _content) in old_state {
		if !new_state.contains_key(id) {
			changes.deleted.push(*id);
		}
	}

	// Find edited messages (in both but content differs)
	for (id, new_content) in new_state {
		if let Some(old_content) = old_state.get(id) {
			if old_content != new_content {
				changes.edited.push((*id, new_content.clone()));
			}
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
		let default_name = format!("group_{}", group_id);
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_parse_file_messages() {
		let content = r#"Hello world <!-- msg:123 -->

## Jan 03
This is a test <!-- msg:456 -->

. Another message <!-- msg:789 -->
"#;

		let messages = parse_file_messages(content);
		assert_eq!(messages.len(), 3);
		assert_eq!(messages.get(&123), Some(&"Hello world".to_string()));
		assert_eq!(messages.get(&456), Some(&"This is a test".to_string()));
		assert_eq!(messages.get(&789), Some(&". Another message".to_string()));
	}

	#[test]
	fn test_detect_changes_deleted() {
		let mut old = BTreeMap::new();
		old.insert(1, "message 1".to_string());
		old.insert(2, "message 2".to_string());

		let mut new = BTreeMap::new();
		new.insert(1, "message 1".to_string());
		// message 2 is deleted

		let changes = detect_changes(&old, &new);
		assert_eq!(changes.deleted, vec![2]);
		assert!(changes.edited.is_empty());
	}

	#[test]
	fn test_detect_changes_edited() {
		let mut old = BTreeMap::new();
		old.insert(1, "message 1".to_string());
		old.insert(2, "message 2".to_string());

		let mut new = BTreeMap::new();
		new.insert(1, "message 1".to_string());
		new.insert(2, "message 2 edited".to_string());

		let changes = detect_changes(&old, &new);
		assert!(changes.deleted.is_empty());
		assert_eq!(changes.edited, vec![(2, "message 2 edited".to_string())]);
	}

	#[test]
	fn test_detect_changes_mixed() {
		let mut old = BTreeMap::new();
		old.insert(1, "message 1".to_string());
		old.insert(2, "message 2".to_string());
		old.insert(3, "message 3".to_string());

		let mut new = BTreeMap::new();
		new.insert(1, "message 1 edited".to_string());
		// message 2 deleted
		new.insert(3, "message 3".to_string());

		let changes = detect_changes(&old, &new);
		assert_eq!(changes.deleted, vec![2]);
		assert_eq!(changes.edited, vec![(1, "message 1 edited".to_string())]);
	}
}
