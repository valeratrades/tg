use std::{io::{Read, Seek, SeekFrom, Write}, path::Path};

use chrono::{DateTime, TimeDelta, Utc};
use eyre::Result;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::TcpListener,
	task::JoinSet,
};
use xattr::FileExt;

use crate::config::{AppConfig, TelegramDestination};

lazy_static! {
	pub static ref VAR_DIR: &'static Path = Path::new("/var/local/tg");
}

#[derive(Clone, Debug, Default, derive_new::new, Deserialize, Serialize)]
pub struct Message {
	pub destination: TelegramDestination,
	pub message: String,
}

#[derive(Clone, Debug, Default, derive_new::new, Deserialize, Serialize)]
pub struct Response {
	pub status: u16,
	pub message: String,
}

/// # Panics
/// On unsuccessful io operations
pub async fn run(config: AppConfig, bot_token: String) -> Result<()> {
	let addr = format!("127.0.0.1:{}", config.localhost_port);
	let listener = TcpListener::bind(&addr).await?;
	println!("Listening on: {}", addr);

	let mut join_set = JoinSet::new();
	while let Ok((mut socket, _)) = listener.accept().await {
		let config = config.clone();
		let bot_token = bot_token.clone();

		join_set.spawn(async move {
			let mut buf = vec![0; 1024];

			while let Ok(n) = socket.read(&mut buf).await {
				if n == 0 {
					return;
				}

				let received = String::from_utf8_lossy(&buf[0..n]);
				dbg!(&received);
				let message: Message = serde_json::from_str(&received).expect("Only the app should send messages");

				if let Err(e) = socket.write_all(b"200").await {
					eprintln!("Failed to send acknowledgment: {}", e);
					return;
				}

				// In the perfect world would store messages in db by the Destination::hash(), but for now writing directly to end repr, using name as id.
				let chat_filepath = crate::chat_filepath(&message.destination.display(&config));
				let last_write_tag: Option<String> = std::fs::File::open(&chat_filepath)
					.ok()
					.and_then(|file| file.get_xattr("user.last_changed").ok())
					.flatten()
					.map(|v| String::from_utf8_lossy(&v).into_owned());
				let last_write_datetime = last_write_tag
					.as_deref()
					.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
					.map(|dt| dt.with_timezone(&chrono::Utc));

				let message_append_repr = format_message_append(&message.message, last_write_datetime, Utc::now());

				let mut file = std::fs::OpenOptions::new()
					.create(true)
					.truncate(false)
					.read(true)
					.write(true)
					.open(&chat_filepath)
					.expect("config is expected to chmod parent dir to give me write access");

				// Trim trailing whitespace
				let mut file_contents = String::new();
				file.read_to_string(&mut file_contents).expect("failed to read file contents");
				let truncate_including_pos = file_contents.trim_end().len() + 1;
				file.set_len(truncate_including_pos as u64).expect("Failed to truncate file");
				file.seek(SeekFrom::End(0)).unwrap();

				file.write_all(message_append_repr.as_bytes()).expect("Failed to write message to file");
				file.set_xattr("user.last_changed", Utc::now().to_rfc3339().as_bytes()).expect("Failed to set xattr");

				let mut retry = true;
				while retry {
					retry = false;
					match send_message(message.clone(), &bot_token).await {
						Ok(_) => (),
						Err(e) => {
							eprintln!("Failed to send message: {}", e);
							//TODO!!!: keep stack of messages with failed sent, then retry sending every 100s
						}
					}
				}
			}
		});
	}

	while let Some(res) = join_set.join_next().await {
		if let Err(e) = res {
			eprintln!("Task failed: {:?}", e);
		}
	}

	Ok(())
}

pub async fn send_message(message: Message, bot_token: &str) -> Result<()> {
	let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
	let mut params = vec![("text", message.message)];
	let destination = &message.destination;
	params.extend(destination.destination_params());
	let client = reqwest::Client::new();
	let res = client.post(&url).form(&params).send().await?;

	println!("{:#?}\nSender: {bot_token}\n{:#?}", res.text().await?, destination);
	Ok(())
}

/// match duration_since_last_write {
///     < 5 minutes => append directly
///     >= 5 minutes && same day => append with a newline before
///     >= 5 minutes && different day => append with a newline before and after
/// }
pub fn format_message_append(message: &str, last_write_datetime: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
	assert!(last_write_datetime.is_none() || last_write_datetime.unwrap() < now);

	let mut prefix = String::new();
	if let Some(last_write_datetime) = last_write_datetime {
		let duration = now.signed_duration_since(last_write_datetime);

		if duration >= TimeDelta::seconds(5 * 60) {
			if now.format("%d/%m").to_string() == last_write_datetime.format("%d/%m").to_string() {
				//HACK
				prefix = "\n".to_string();
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

  1970-01-03 15:49:00
  1970-01-03 15:50:00

  1970-01-03 16:56:00

  1970-01-03 17:08:00

  1970-01-03 17:19:00
  1970-01-03 17:20:00

  1970-01-03 17:33:00

  1970-01-03 20:36:00
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

	#[test]
	fn chat_filepath() -> Result<()> {
		let destination = TelegramDestination::Group { id: 2244305221, thread_id: 3 };

		let mut config = AppConfig::default();
		config.channels.insert("test".to_string(), destination);

		let chat_filepath = crate::chat_filepath(&destination.display(&config));
		insta::assert_debug_snapshot!(chat_filepath, @r###""/var/local/tg/test.md""###);
		Ok(())
	}
}
