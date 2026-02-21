use eyre::Result;
use jiff::civil::Date;

use crate::config::{LiveSettings, TopicsMetadata};
use crate::pull::topic_filepath;

struct RawMessage {
	message_id: i32,
	date: Date,
	content: String,
	is_voice: bool,
	group_name: String,
	topic_name: String,
}

pub async fn run(count: usize, _config: &LiveSettings) -> Result<()> {
	let metadata = TopicsMetadata::load();
	let today = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date();
	let current_year = today.year();

	let mut all_messages: Vec<RawMessage> = Vec::new();

	for (group_id, group) in &metadata.groups {
		let group_name = group.name.as_deref().unwrap_or(&format!("group_{group_id}")).to_string();

		for (topic_id, topic_name) in &group.topics {
			let file_path = topic_filepath(*group_id, *topic_id, &metadata);
			let content = match std::fs::read_to_string(&file_path) {
				Ok(c) => c,
				Err(_) => continue,
			};

			let date_map = crate::build_message_date_map(&content, current_year, today);
			let parsed = crate::sync::parse_file_messages(&content);

			for (msg_id, parsed_msg) in parsed {
				let date = date_map.get(&msg_id).copied().unwrap_or(today);
				all_messages.push(RawMessage {
					message_id: msg_id,
					date,
					content: parsed_msg.content,
					is_voice: parsed_msg.is_voice,
					group_name: group_name.clone(),
					topic_name: topic_name.clone(),
				});
			}
		}
	}

	all_messages.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.message_id.cmp(&b.message_id)));

	let start = all_messages.len().saturating_sub(count);
	let last_n = &all_messages[start..];

	// Group consecutive same-topic messages
	let mut groups: Vec<(String, String, Vec<&RawMessage>)> = Vec::new();
	for msg in last_n {
		let matches_prev = groups.last().is_some_and(|(g, t, _)| g == &msg.group_name && t == &msg.topic_name);
		if matches_prev {
			groups.last_mut().unwrap().2.push(msg);
		} else {
			groups.push((msg.group_name.clone(), msg.topic_name.clone(), vec![msg]));
		}
	}

	for (i, (group_name, topic_name, messages)) in groups.iter().enumerate() {
		if i > 0 {
			println!();
		}
		println!("## {group_name}/{topic_name}");
		for msg in messages {
			let display_content = format_display_content(&msg.content, msg.is_voice);
			println!("[{} UTC] {display_content}", msg.date.strftime("%Y-%m-%d 00:00"));
		}
	}

	Ok(())
}

fn format_display_content(content: &str, is_voice: bool) -> String {
	if is_voice {
		// Strip [voice] prefix
		let text = content.strip_prefix("[voice] ").unwrap_or(content);
		return text.to_string();
	}
	content.to_string()
}
