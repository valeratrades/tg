use std::{
	io::{Read, Seek, SeekFrom, Write},
	path::PathBuf,
	sync::OnceLock,
};

use chrono::{DateTime, TimeDelta, Utc};
use eyre::Result;
use serde::{Deserialize, Serialize};
use tg::chat::TelegramDestination;
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::TcpListener,
	task::JoinSet,
};
use tracing::{debug, error, info, instrument, warn};
use xattr::FileExt as _;

use crate::config::AppConfig;

pub static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

#[derive(Clone, Debug, Default, derive_new::new, Deserialize, Serialize)]
pub struct Message {
	pub destination: TelegramDestination,
	pub message: String,
}

/// # Panics
/// On unsuccessful io operations
#[instrument(skip(bot_token), fields(port = config.localhost_port))]
pub async fn run(config: AppConfig, bot_token: String) -> Result<()> {
	info!("Starting telegram server");
	let addr = format!("127.0.0.1:{}", config.localhost_port);
	debug!("Binding to address: {}", addr);
	let listener = TcpListener::bind(&addr).await?;
	info!("Listening on: {}", addr);

	let mut join_set = JoinSet::new();
	while let Ok((mut socket, addr)) = listener.accept().await {
		info!("Accepted connection from: {}", addr);
		let config = config.clone();
		let bot_token = bot_token.clone();

		join_set.spawn(async move {
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

				// In the perfect world would store messages in db by the Destination::hash(), but for now writing directly to end repr, using name as id.
				let destination_name = crate::config::display_destination(&message.destination, &config);
				let chat_filepath = crate::chat_filepath(&destination_name);
				info!("Processing message for destination '{}' to file: {}", destination_name, chat_filepath.display());

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

				// Trim trailing whitespace
				let mut file_contents = String::new();
				if let Err(e) = file.read_to_string(&mut file_contents) {
					error!("Failed to read file contents: {}", e);
					return;
				}
				let truncate_including_pos = file_contents.trim_end().len() + 1;
				if let Err(e) = file.set_len(truncate_including_pos as u64) {
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

				let mut retry = true;
				while retry {
					retry = false;
					match send_message(message.clone(), &bot_token).await {
						Ok(_) => {
							info!("Message sent successfully to Telegram");
						}
						Err(e) => {
							error!("Failed to send message to Telegram: {}", e);
							//TODO!!!: keep stack of messages with failed sent, then retry sending every 100s
						}
					}
				}
			}
		});
	}

	info!("Server shutting down, waiting for tasks to complete");
	while let Some(res) = join_set.join_next().await {
		if let Err(e) = res {
			error!("Task failed: {:?}", e);
		}
	}

	info!("Server stopped");
	Ok(())
}

#[instrument(skip(bot_token), fields(destination = ?message.destination))]
pub async fn send_message(message: Message, bot_token: &str) -> Result<()> {
	debug!("Sending message to Telegram API");
	let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
	let mut params = vec![("text", message.message.clone())];
	let destination = &message.destination;
	params.extend(destination.destination_params());

	let client = reqwest::Client::new();
	debug!("Posting to Telegram API");
	let res = client.post(&url).form(&params).send().await?;

	let status = res.status();
	let response_text = res.text().await?;

	if status.is_success() {
		debug!("Telegram API response: {}", response_text);
		info!("Successfully sent message to {:?}", destination);
	} else {
		warn!("Telegram API returned non-success status {}: {}", status, response_text);
	}

	Ok(())
}

/// match duration_since_last_write {
///     < 6 minutes => append directly
///     >= 6 minutes && same day => append with a newline before
///     >= 6 minutes && different day => append with a newline before and after
/// }
#[instrument(skip(message), fields(message_len = message.len()))]
pub fn format_message_append(message: &str, last_write_datetime: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
	debug!("Formatting message append");
	assert!(last_write_datetime.is_none() || last_write_datetime.unwrap() < now);

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

	format!("{}{}\n", prefix, message)
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
		let destination_str_source = TelegramDestination::Group { id: 2244305221, thread_id: 3 };
		let destination_str = serde_json::to_string(&destination_str_source)?;
		let destination: TelegramDestination = serde_json::from_str(&destination_str)?;
		let destination_reserialized = serde_json::to_string(&destination)?;
		assert_eq!(destination_str, destination_reserialized);

		let message_str = format!(r#"{{"destination":{},"message":"a message"}}"#, destination_str);
		let message: Message = serde_json::from_str(&message_str)?;
		insta::assert_debug_snapshot!(message, @r###"
  Message {
      destination: Group {
          id: 2244305221,
          thread_id: 3,
      },
      message: "a message",
  }
  "###);
		Ok(())
	}

	// that's an integration test. TODO: move
	//#[test]
	//fn chat_filepath() -> Result<()> {
	//	let destination = TelegramDestination::Group { id: 2244305221, thread_id: 3 };
	//
	//	let mut config = AppConfig::default();
	//	config.channels.insert("test".to_string(), destination);
	//
	//	let chat_filepath = crate::chat_filepath(&destination.display(&config));
	//	insta::assert_debug_snapshot!(chat_filepath, @r###""/home/v/tg/test.md""###);
	//	Ok(())
	//}
}
