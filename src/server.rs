use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use crate::config::AppConfig;
use lazy_static::lazy_static;
use std::path::Path;
use std::io::Write;

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

pub async fn run(config: crate::config::AppConfig, bot_token: String) -> Result<()> {
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
				let message = serde_json::from_str::<Message>(&received).expect("Only the app should send messages");

				if let Err(e) = socket.write_all(b"200").await {
					eprintln!("Failed to send acknowledgment: {}", e);
					return;
				}

				// In the perfect world would store messages in db by the Destination::hash(), but for now writing directly to end repr, using name as id.
				let chat_filepath = VAR_DIR.join(format!("{}.toml", &message.destination));
				let message_append_repr = format_message_append(&message.message);
				std::fs::OpenOptions::new()
					.create(true)
					.append(true)
					.open(chat_filepath).expect("config is expected to chmod parent dir to give me write access")
					.write_all(message_append_repr.as_bytes()).expect("Failed to write message to file");

				if let Err(e) = send_message(&config, message, &bot_token).await {
					eprintln!("Failed to send message: {}", e);
					//TODO!!!: keep stack of messages with failed sent, then retry sending every 100s
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

pub fn format_message_append(message: &str) -> String {
	format!("{}\n", message)
}

pub async fn send_message(config: &AppConfig, message: Message, bot_token: &str) -> Result<()> {
	let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
	let mut params = vec![("text", message.message)];
	let destination = config.channels.get(&message.destination).expect("already checked on cli evocation");
	params.extend(destination.destination_params());
	let client = reqwest::Client::new();
	let res = client.post(&url).form(&params).send().await?;

	println!("{:#?}\nSender: {bot_token}\n{:#?}", res.text().await?, destination);
	Ok(())
}
