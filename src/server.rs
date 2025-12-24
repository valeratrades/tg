use std::{
	io::{Read, Seek, SeekFrom, Write},
	path::PathBuf,
	pin::{Pin, pin},
	sync::OnceLock,
};

use chrono::{DateTime, TimeDelta, Utc};
use eyre::Result;
use futures::future::{Either, select};
use futures_util::{StreamExt as _, stream::FuturesUnordered};
use serde::{Deserialize, Serialize};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::{TcpListener, TcpStream},
	time::Interval,
};
use tracing::{debug, error, info, warn};
use v_utils::trades::Timeframe;
use xattr::FileExt as _;

use crate::{
	config::{AppConfig, TopicsMetadata, telegram_chat_id},
	pull::{self, topic_filepath},
};

pub static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Message to send to a specific topic in a forum group
#[derive(Clone, Debug, Default, Deserialize, Serialize, derive_new::new)]
pub struct Message {
	pub group_id: u64,
	pub topic_id: u64,
	pub message: String,
}

/// Result from completing a task
enum TaskResult {
	/// Connection finished (closed or error)
	ConnectionDone,
	/// Pull tick completed, return the interval to re-queue
	PullDone(Interval),
}

/// # Panics
/// On unsuccessful io operations
pub async fn run(config: AppConfig, bot_token: String, pull_interval: Timeframe) -> Result<()> {
	info!("Starting telegram server");

	// Discover forum topics and create topic files at startup
	info!("Discovering forum topics for configured groups...");
	pull::discover_and_create_topic_files(&config, &bot_token).await?;

	let addr = format!("127.0.0.1:{}", config.localhost_port);
	debug!("Binding to address: {}", addr);
	let listener = TcpListener::bind(&addr).await?;
	info!("Listening on: {}", addr);

	let interval_duration = pull_interval.duration();
	info!("Pull interval: {}", pull_interval);

	type BoxFut = Pin<Box<dyn std::future::Future<Output = (TaskResult, AppConfig, String)> + Send>>;
	let mut futures: FuturesUnordered<BoxFut> = FuturesUnordered::new();

	// Initial pull task
	let pull_interval_timer = tokio::time::interval(interval_duration);
	let config_clone = config.clone();
	let token_clone = bot_token.clone();
	futures.push(Box::pin(async move {
		let mut interval = pull_interval_timer;
		interval.tick().await;

		match pull::pull(&config_clone, &token_clone).await {
			Ok(()) => debug!("Pull completed successfully"),
			Err(e) => warn!("Pull failed: {}", e),
		}

		(TaskResult::PullDone(interval), config_clone, token_clone)
	}));

	loop {
		enum Event {
			NewConnection(std::io::Result<(TcpStream, std::net::SocketAddr)>),
			TaskCompleted(Option<(TaskResult, AppConfig, String)>),
		}

		let event = {
			let accept_fut = pin!(listener.accept());
			let task_fut = pin!(futures.next());

			match select(accept_fut, task_fut).await {
				Either::Left((accept_result, _task_fut)) => Event::NewConnection(accept_result),
				Either::Right((task_result, _accept_fut)) => Event::TaskCompleted(task_result),
			}
		};

		match event {
			Event::NewConnection(accept_result) => {
				let (socket, addr) = accept_result?;
				info!("Accepted connection from: {}", addr);

				let config_clone = config.clone();
				let token_clone = bot_token.clone();
				futures.push(Box::pin(async move {
					handle_connection(socket, &config_clone, &token_clone).await;
					(TaskResult::ConnectionDone, config_clone, token_clone)
				}));
			}
			Event::TaskCompleted(Some((result, config_ret, token_ret))) => match result {
				TaskResult::ConnectionDone => {
					debug!("Connection task completed");
				}
				TaskResult::PullDone(mut interval) => {
					debug!("Pull task completed, re-queuing");
					let config_clone = config_ret;
					let token_clone = token_ret;
					futures.push(Box::pin(async move {
						interval.tick().await;

						match pull::pull(&config_clone, &token_clone).await {
							Ok(()) => debug!("Pull completed successfully"),
							Err(e) => warn!("Pull failed: {}", e),
						}

						(TaskResult::PullDone(interval), config_clone, token_clone)
					}));
				}
			},
			Event::TaskCompleted(None) => {
				// FuturesUnordered is empty - shouldn't happen since pull is always re-queued
				warn!("All tasks completed unexpectedly");
				break;
			}
		}
	}

	info!("Server stopped");
	Ok(())
}

async fn handle_connection(mut socket: TcpStream, _config: &AppConfig, bot_token: &str) {
	let mut buf = vec![0; 1024];

	while let Ok(n) = socket.read(&mut buf).await {
		if n == 0 {
			debug!("Connection closed by client");
			return;
		}

		let received = String::from_utf8_lossy(&buf[0..n]);
		debug!("Received raw message: {}", received);
		let message: Message = match serde_json::from_str(&received) {
			Ok(m) => m,
			Err(e) => {
				error!("Failed to deserialize message: {}. Raw: {}", e, received);
				return;
			}
		};

		if let Err(e) = socket.write_all(b"200").await {
			error!("Failed to send acknowledgment: {}", e);
			return;
		}
		debug!("Sent acknowledgment");

		let metadata = TopicsMetadata::load();
		let chat_filepath = topic_filepath(message.group_id, message.topic_id, &metadata);
		info!(
			"Processing message for group {} topic {} to file: {}",
			message.group_id,
			message.topic_id,
			chat_filepath.display()
		);

		let last_write_tag: Option<String> = std::fs::File::open(&chat_filepath)
			.ok()
			.and_then(|file| {
				debug!("Reading xattr from existing file");
				file.get_xattr("user.last_changed").ok()
			})
			.flatten()
			.map(|v| String::from_utf8_lossy(&v).into_owned());
		let last_write_datetime = last_write_tag
			.as_deref()
			.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).inspect_err(|e| warn!("Failed to parse last_changed xattr: {}", e)).ok())
			.map(|dt| dt.with_timezone(&chrono::Utc));

		let message_append_repr = format_message_append(&message.message, last_write_datetime, Utc::now());
		debug!("Formatted message append: {:?}", message_append_repr);

		// Ensure the parent directory exists
		if let Some(parent) = chat_filepath.parent() {
			if let Err(e) = std::fs::create_dir_all(parent) {
				error!("Failed to create directory '{}': {}", parent.display(), e);
				return;
			}
		}

		let mut file = match std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&chat_filepath) {
			Ok(f) => {
				debug!("Opened chat file successfully");
				f
			}
			Err(e) => {
				error!("Failed to open chat file '{}': {}", chat_filepath.display(), e);
				return;
			}
		};

		// Trim trailing whitespace, keeping at most one newline
		let mut file_contents = String::new();
		if let Err(e) = file.read_to_string(&mut file_contents) {
			error!("Failed to read file contents: {}", e);
			return;
		}
		let trimmed_len = file_contents.trim_end().len();
		// Cap at file length to avoid extending the file if it doesn't end with whitespace
		let truncate_to = std::cmp::min(trimmed_len + 1, file_contents.len());
		if let Err(e) = file.set_len(truncate_to as u64) {
			error!("Failed to truncate file: {}", e);
			return;
		}
		if let Err(e) = file.seek(SeekFrom::End(0)) {
			error!("Failed to seek to end of file: {}", e);
			return;
		}

		if let Err(e) = file.write_all(message_append_repr.as_bytes()) {
			error!("Failed to write message to file: {}", e);
			return;
		}
		debug!("Wrote message to file");

		let now_rfc3339 = Utc::now().to_rfc3339();
		if let Err(e) = file.set_xattr("user.last_changed", now_rfc3339.as_bytes()) {
			error!("Failed to set xattr: {}", e);
			return;
		}
		debug!("Set xattr successfully");

		let mut delay_secs = 1.0_f64;
		const E: f64 = std::f64::consts::E;
		const THIRTY_MINS_SECS: f64 = 30.0 * 60.0;

		loop {
			match send_message(&message, bot_token).await {
				Ok(msg_id) => {
					info!("Message sent successfully to Telegram with id {}", msg_id);

					// Update the local file to add the message ID marker
					if let Err(e) = append_msg_id_to_last_line(&chat_filepath, msg_id) {
						warn!("Failed to add message ID marker to file: {}", e);
					}
					break;
				}
				Err(e) => {
					let total_elapsed_secs = (delay_secs - 1.0) * (E - 1.0).recip() * E; // approximate total time spent retrying
					if total_elapsed_secs >= THIRTY_MINS_SECS {
						error!("Failed to send message to Telegram after 30+ minutes: {}", e);
					} else {
						warn!("Failed to send message to Telegram, retrying in {:.0}s: {}", delay_secs, e);
					}
					tokio::time::sleep(std::time::Duration::from_secs_f64(delay_secs)).await;
					delay_secs *= E;
				}
			}
		}
	}
}

/// Append a message ID marker to the last non-empty line of a file
fn append_msg_id_to_last_line(path: &std::path::Path, msg_id: i32) -> Result<()> {
	let content = std::fs::read_to_string(path)?;
	let lines: Vec<&str> = content.lines().collect();

	// Find the last non-empty line and append the marker
	for i in (0..lines.len()).rev() {
		let line = lines[i].trim();
		if !line.is_empty() && !line.starts_with("## ") && !line.starts_with(". ") {
			// This is likely the message line we just wrote
			let marker = format!(" <!-- msg:{} -->", msg_id);
			let new_line = format!("{}{}", lines[i].trim_end(), marker);
			let mut result: Vec<String> = lines[..i].iter().map(|s| s.to_string()).collect();
			result.push(new_line);
			result.extend(lines[i + 1..].iter().map(|s| s.to_string()));
			std::fs::write(path, result.join("\n") + "\n")?;
			return Ok(());
		}
	}

	Ok(())
}

/// Extract image path from markdown image syntax: ![alt](path) or ![](path)
fn extract_image_path(text: &str) -> Option<(String, Option<String>)> {
	// Match ![...](...) pattern
	let re = regex::Regex::new(r"^!\[([^\]]*)\]\(([^)]+)\)").ok()?;
	let caps = re.captures(text.trim())?;
	let path = caps.get(2)?.as_str().to_string();
	// Get remaining text after the image tag as caption
	let full_match = caps.get(0)?;
	let remaining = text[full_match.end()..].trim();
	let caption = if remaining.is_empty() { None } else { Some(remaining.to_string()) };
	Some((path, caption))
}

/// Returns the message ID assigned by Telegram
pub async fn send_message(message: &Message, bot_token: &str) -> Result<i32> {
	debug!("Sending message to Telegram API");
	let client = reqwest::Client::new();

	let chat_id = telegram_chat_id(message.group_id);

	// Check if message contains an image
	if let Some((image_path, caption)) = extract_image_path(&message.message) {
		// Resolve the image path relative to data directory
		let full_path = DATA_DIR.get().unwrap().join(&image_path);

		if full_path.exists() {
			debug!("Sending photo from {}", full_path.display());
			let url = format!("https://api.telegram.org/bot{}/sendPhoto", bot_token);

			// Read image file
			let image_bytes = std::fs::read(&full_path)?;
			let filename = full_path.file_name().and_then(|n| n.to_str()).unwrap_or("image.jpg").to_string();

			// Build multipart form
			let mut form = reqwest::multipart::Form::new()
				.part("photo", reqwest::multipart::Part::bytes(image_bytes).file_name(filename))
				.text("chat_id", chat_id.to_string());

			// General topic (id=1) doesn't use message_thread_id
			if message.topic_id != 1 {
				form = form.text("message_thread_id", message.topic_id.to_string());
			}

			// Add caption if present
			if let Some(cap) = caption {
				form = form.text("caption", cap);
			}

			let res = client.post(&url).multipart(form).send().await?;
			let status = res.status();
			let response_text = res.text().await?;

			if status.is_success() {
				debug!("Telegram API response: {}", response_text);
				info!("Successfully sent photo to group {} topic {}", message.group_id, message.topic_id);

				// Extract message_id from response
				let response: serde_json::Value = serde_json::from_str(&response_text)?;
				let msg_id = response["result"]["message_id"].as_i64().ok_or_else(|| eyre::eyre!("No message_id in response"))? as i32;
				Ok(msg_id)
			} else {
				warn!("Telegram API returned non-success status {}: {}", status, response_text);
				Err(eyre::eyre!("Telegram API error: {}", status))
			}
		} else {
			warn!("Image file not found: {}, sending as text", full_path.display());
			send_text_message(&client, &message.message, message.group_id, message.topic_id, bot_token).await
		}
	} else {
		send_text_message(&client, &message.message, message.group_id, message.topic_id, bot_token).await
	}
}

/// Returns the message ID assigned by Telegram
async fn send_text_message(client: &reqwest::Client, text: &str, group_id: u64, topic_id: u64, bot_token: &str) -> Result<i32> {
	let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
	let chat_id = telegram_chat_id(group_id);

	// General topic (id=1) doesn't use message_thread_id
	let mut params = vec![("text", text.to_string()), ("chat_id", chat_id.to_string())];
	if topic_id != 1 {
		params.push(("message_thread_id", topic_id.to_string()));
	}

	debug!("Posting text to Telegram API");
	let res = client.post(&url).form(&params).send().await?;

	let status = res.status();
	let response_text = res.text().await?;

	if status.is_success() {
		debug!("Telegram API response: {}", response_text);
		info!("Successfully sent message to group {} topic {}", group_id, topic_id);

		// Extract message_id from response
		let response: serde_json::Value = serde_json::from_str(&response_text)?;
		let msg_id = response["result"]["message_id"].as_i64().ok_or_else(|| eyre::eyre!("No message_id in response"))? as i32;
		Ok(msg_id)
	} else {
		warn!("Telegram API returned non-success status {}: {}", status, response_text);
		Err(eyre::eyre!("Telegram API error: {}", status))
	}
}

/// match duration_since_last_write {
///     < 6 minutes => append directly
///     >= 6 minutes && same day => append with a newline before
///     >= 6 minutes && different day => append with a newline before and after
/// }
pub fn format_message_append(message: &str, last_write_datetime: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
	format_message_append_with_id(message, last_write_datetime, now, None)
}

/// Format a message append with an optional message ID marker
pub fn format_message_append_with_id(message: &str, last_write_datetime: Option<DateTime<Utc>>, now: DateTime<Utc>, msg_id: Option<i32>) -> String {
	debug!("Formatting message append");
	// Note: last_write_datetime can be > now when processing historical messages from Telegram
	// (message timestamps may be slightly ahead due to server time differences)

	let mut prefix = String::new();
	if let Some(last_write_datetime) = last_write_datetime {
		let duration = now.signed_duration_since(last_write_datetime);

		if duration >= TimeDelta::minutes(6) {
			if now.format("%d/%m").to_string() == last_write_datetime.format("%d/%m").to_string() {
				prefix = "\n. ".to_string();
			} else {
				prefix = format!("\n## {}\n", now.format("%b %d"));
			}
		}
	}

	let id_suffix = msg_id.map(|id| format!(" <!-- msg:{} -->", id)).unwrap_or_default();
	format!("{}{}{}\n", prefix, message, id_suffix)
}

#[cfg(test)]
mod tests {
	use eyre::Result;
	use v_utils::distributions::ReimanZeta;

	use super::*;

	#[test]
	fn test_format_message_append() {
		let mut messages = Vec::new();

		let zeta_dist = ReimanZeta::new(1.0, 4320);

		let mut accumulated_offset = DateTime::UNIX_EPOCH;
		for i in 0..10 {
			let time_offset_mins = zeta_dist.sample(Some(i));
			let time_offset = TimeDelta::seconds(time_offset_mins as i64 * 60);
			accumulated_offset += time_offset;
			let message = accumulated_offset.format("%Y-%m-%d %H:%M:%S").to_string(); // for ease of testing

			messages.push((message, accumulated_offset));
		}

		messages.sort_by_key(|&(_, timestamp)| timestamp);

		let mut formatted_messages = String::new();
		for (i, (m, t)) in messages.iter().enumerate() {
			let last_change = if i == 0 { None } else { Some(messages[i - 1].1) };
			let m = format_message_append(m, last_change, *t);
			formatted_messages.push_str(&m);
		}

		insta::assert_snapshot!(formatted_messages, @r###"
  1970-01-01 06:30:00

  ## Jan 03
  1970-01-03 15:41:00

  . 1970-01-03 15:49:00
  1970-01-03 15:50:00

  . 1970-01-03 16:56:00

  . 1970-01-03 17:08:00

  . 1970-01-03 17:19:00
  1970-01-03 17:20:00

  . 1970-01-03 17:33:00

  . 1970-01-03 20:36:00
  "###);
	}

	#[test]
	fn deser_message() -> Result<()> {
		let message_str = r#"{"group_id":2244305221,"topic_id":3,"message":"a message"}"#;
		let message: Message = serde_json::from_str(message_str)?;
		insta::assert_debug_snapshot!(message, @r###"
  Message {
      group_id: 2244305221,
      topic_id: 3,
      message: "a message",
  }
  "###);
		Ok(())
	}
}
