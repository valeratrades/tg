use std::io::Write as _;

use eyre::Result;
use jiff::civil::Date;

use crate::{
	config::{LiveSettings, TopicsMetadata},
	pull::topic_filepath,
};

pub async fn run(count: usize, config: &LiveSettings) -> Result<()> {
	let mut all_messages = load_all_messages();

	all_messages.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.message_id.cmp(&b.message_id)));

	let start = all_messages.len().saturating_sub(count);
	let last_n = &all_messages[start..];

	if !confirm_large_output(last_n.len(), config)? {
		return Ok(());
	}

	print_message_groups(&group_consecutive(last_n));

	Ok(())
}

/// Prompt for confirmation if message count exceeds the configured threshold.
/// Returns true to proceed, false to abort.
pub(crate) fn confirm_large_output(count: usize, config: &LiveSettings) -> Result<bool> {
	let threshold = config.config()?.print_confirm_threshold();
	if count <= threshold {
		return Ok(true);
	}
	eprint!("{count} messages matched. Continue? [y/N] ");
	std::io::stderr().flush()?;
	let mut input = String::new();
	std::io::stdin().read_line(&mut input)?;
	Ok(input.trim().eq_ignore_ascii_case("y"))
}

pub(crate) struct RawMessage {
	pub message_id: i32,
	pub date: Date,
	/// UTC unix timestamp from the message tag, if available.
	pub ts: Option<i64>,
	pub content: String,
	pub is_voice: bool,
	pub group_name: String,
	pub topic_name: String,
}

/// Load all messages from all topic files.
pub(crate) fn load_all_messages() -> Vec<RawMessage> {
	let metadata = TopicsMetadata::load();
	let today = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date();
	let current_year = today.year();

	let mut all_messages = Vec::new();

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
					ts: parsed_msg.ts,
					content: parsed_msg.content,
					is_voice: parsed_msg.is_voice,
					group_name: group_name.clone(),
					topic_name: topic_name.clone(),
				});
			}
		}
	}

	all_messages
}

/// Group consecutive same-topic messages together.
pub(crate) fn group_consecutive(messages: &[RawMessage]) -> Vec<(&str, &str, Vec<&RawMessage>)> {
	let mut groups: Vec<(&str, &str, Vec<&RawMessage>)> = Vec::new();
	for msg in messages {
		let matches_prev = groups.last().is_some_and(|(g, t, _)| *g == msg.group_name && *t == msg.topic_name);
		if matches_prev {
			groups.last_mut().unwrap().2.push(msg);
		} else {
			groups.push((&msg.group_name, &msg.topic_name, vec![msg]));
		}
	}
	groups
}

pub(crate) fn print_message_groups(groups: &[(&str, &str, Vec<&RawMessage>)]) {
	for (i, (group_name, topic_name, messages)) in groups.iter().enumerate() {
		if i > 0 {
			println!();
		}
		println!("## {group_name}/{topic_name}");
		for msg in messages {
			let display_content = format_display_content(&msg.content, msg.is_voice);
			let time_str = format_message_time(msg);
			println!("[{time_str}] {display_content}");
		}
	}
}

fn format_message_time(msg: &RawMessage) -> String {
	match msg.ts {
		Some(ts) => {
			let timestamp = jiff::Timestamp::from_second(ts).expect("valid unix timestamp");
			let zoned = timestamp.to_zoned(jiff::tz::TimeZone::UTC);
			format!("{} UTC", zoned.strftime("%Y-%m-%d %H:%M"))
		}
		#[allow(deprecated)]
		None => format_message_time_legacy(msg.date),
	}
}

/// Format timestamp for messages without embedded `ts:` in their tag.
/// These predate ts: support and only have date-level granularity from file headers.
#[deprecated(since = "1.0.0", note = "all messages should have ts: in their tag by now")]
fn format_message_time_legacy(date: Date) -> String {
	format!("{} UNKNOWN UTC", date.strftime("%Y-%m-%d"))
}

fn format_display_content(content: &str, is_voice: bool) -> String {
	if is_voice {
		let text = content.strip_prefix("[voice] ").unwrap_or(content);
		return text.to_string();
	}
	content.to_string()
}
