use crate::config::AppConfig;
use anyhow::Result;
use chrono::{DateTime, Local};
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinSet;

lazy_static! {
	pub static ref VAR_DIR: &'static Path = Path::new("/var/local/tg");
}

#[derive(Clone, Debug, Default, derive_new::new, Deserialize, Serialize)]
pub struct Message {
	pub destination: String,
	pub message: String,
}

#[derive(Clone, Debug, Default, derive_new::new, Deserialize, Serialize)]
pub struct Response {
	pub status: u16,
	pub message: String,
}

pub async fn run(config: crate::config::AppConfig, bot_token: String, config_path: &Path) -> Result<()> {
	let addr = format!("127.0.0.1:{}", config.localhost_port);
	let listener = TcpListener::bind(&addr).await?;
	println!("Listening on: {}", addr);

	let mut join_set = JoinSet::new();
	while let Ok((mut socket, _)) = listener.accept().await {
		let mut config = config.clone();
		let bot_token = bot_token.clone();

		let config_path = config_path.to_path_buf();
		join_set.spawn(async move {
			let mut buf = vec![0; 1024];

			while let Ok(n) = socket.read(&mut buf).await {
				if n == 0 {
					return;
				}

				let received = String::from_utf8_lossy(&buf[0..n]);
				let message = serde_json::from_str::<Message>(&received).expect("Only the app should send messages");

				if let Err(e) = socket.write_all(b"200").await {
					eprintln!("Failed to send acknowledgment: {}", e);
					return;
				}

				// In the perfect world would store messages in db by the Destination::hash(), but for now writing directly to end repr, using name as id.
				let chat_filepath = crate::chat_filepath(&message.destination);
				let last_modified: Option<SystemTime> = chat_filepath.metadata().ok().and_then(|metadata| metadata.modified().ok());

				let message_append_repr = format_message_append(&message.message, last_modified, SystemTime::now());
				std::fs::OpenOptions::new()
					.create(true)
					.append(true)
					.open(chat_filepath)
					.expect("config is expected to chmod parent dir to give me write access")
					.write_all(message_append_repr.as_bytes())
					.expect("Failed to write message to file");

				let mut retry = true;
				while retry {
					retry = false;
					match send_message(&config, message.clone(), &bot_token).await {
						Ok(_) => (),
						Err(SendError::ConfigOutOfSync) => {
							config = crate::config::AppConfig::read(&config_path).expect("Failed to read config file");
							retry = true;
						}
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

#[derive(thiserror::Error, Debug)]
pub enum SendError {
	#[error("Config out of sync")]
	ConfigOutOfSync,
	#[error(transparent)]
	Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}
pub async fn send_message(config: &AppConfig, message: Message, bot_token: &str) -> Result<(), SendError> {
	let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
	let mut params = vec![("text", message.message)];
	let destination = config.channels.get(&message.destination).ok_or(SendError::ConfigOutOfSync)?;
	params.extend(destination.destination_params());
	let client = reqwest::Client::new();
	let res = client.post(&url).form(&params).send().await.map_err(|e| SendError::Other(Box::new(e)))?;

	println!(
		"{:#?}\nSender: {bot_token}\n{:#?}",
		res.text().await.map_err(|e| SendError::Other(Box::new(e))),
		destination
	);
	Ok(())
}

pub fn format_message_append(message: &str, time_of_last_change: Option<SystemTime>, now: SystemTime) -> String {
	assert!(time_of_last_change.is_none() || time_of_last_change.unwrap() < now);
	let prefix = time_of_last_change
		.and_then(|last_change| {
			now.duration_since(last_change).ok().map(|duration| {
				if duration < Duration::from_secs(5 * 60) {
					String::new()
				} else {
					let now_local: DateTime<Local> = now.into();
					let last_change_local: DateTime<Local> = last_change.into();

					dbg!(&now_local.format("%d/%m").to_string(), &last_change_local.format("%d/%m").to_string());
					if now_local.format("%d/%m").to_string() == last_change_local.format("%d/%m").to_string() {
						//HACK
						"\n".to_string()
					} else {
						format!("\n    {}\n", now_local.format("%b %d"))
					}
				}
			})
		})
		.unwrap_or_default();

	format!("{}{}\n", prefix, message)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::utils::ZetaDistribution;

	#[test]
	fn test_format_message_append() {
		let mut messages = Vec::new();

		let zeta_dist = ZetaDistribution::new(1.0, 4320);

		let mut accumulated_offset = std::time::UNIX_EPOCH;
		for i in 0..10 {
			let time_offset_mins = zeta_dist.sample(Some(i)) as u64;
			let time_offset = Duration::from_secs(time_offset_mins * 60);
			accumulated_offset += time_offset;
			let datetime: DateTime<Local> = accumulated_offset.into();
			let message = datetime.format("%Y-%m-%d %H:%M:%S").to_string();

			messages.push((message, accumulated_offset));
		}

		messages.sort_by_key(|&(_, timestamp)| timestamp);

		let mut formatted_messages = String::new();
		for (i, (message, timestamp)) in messages.iter().enumerate() {
			let last_change = if i == 0 { None } else { Some(messages[i - 1].1) };
			let m = format_message_append(message, last_change, *timestamp);
			formatted_messages.push_str(&m);
		}

		insta::assert_snapshot!(formatted_messages, @r###"
  1970-01-01 06:30:00

      Jan 03
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
}
