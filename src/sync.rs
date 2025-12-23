use std::{collections::BTreeMap, io::Write as _, path::Path};

use eyre::Result;
use grammers_client::Client;
use regex::Regex;
use tracing::{debug, info, warn};

use crate::{config::TopicsMetadata, mtproto};

/// Represents changes detected between old and new file states
#[derive(Clone, Debug, Default)]
pub struct FileChanges {
	/// Messages that were deleted (message_id)
	pub deleted: Vec<i32>,
	/// Messages that were edited (message_id, new_content)
	pub edited: Vec<(i32, String)>,
}

impl FileChanges {
	pub fn total_affected(&self) -> usize {
		self.deleted.len() + self.edited.len()
	}

	pub fn is_empty(&self) -> bool {
		self.deleted.is_empty() && self.edited.is_empty()
	}
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

/// Apply changes to Telegram via MTProto, with confirmation if many messages affected
pub async fn apply_changes(changes: &FileChanges, group_id: u64, client: &Client) -> Result<()> {
	if changes.is_empty() {
		debug!("No changes to apply");
		return Ok(());
	}

	let total = changes.total_affected();
	info!("Applying {} changes ({} deletions, {} edits)", total, changes.deleted.len(), changes.edited.len());

	// Sanity check for many changes
	if total > 25 {
		print!("About to modify {} messages on Telegram. Continue? [y/N] ", total);
		std::io::stdout().flush()?;
		let mut input = String::new();
		std::io::stdin().read_line(&mut input)?;
		if !input.trim().eq_ignore_ascii_case("y") {
			info!("Aborted by user");
			return Ok(());
		}
	}

	// Apply deletions (batch delete)
	if !changes.deleted.is_empty() {
		match mtproto::delete_messages(client, group_id, &changes.deleted).await {
			Ok(count) => info!("Deleted {} message(s)", count),
			Err(e) => warn!("Failed to delete messages: {}", e),
		}
	}

	// Apply edits (one by one)
	for (msg_id, new_content) in &changes.edited {
		match mtproto::edit_message(client, group_id, *msg_id, new_content).await {
			Ok(()) => info!("Edited message {}", msg_id),
			Err(e) => warn!("Failed to edit message {}: {}", msg_id, e),
		}
	}

	info!("Changes applied successfully");
	Ok(())
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
