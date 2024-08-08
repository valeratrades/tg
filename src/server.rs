use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Debug, Default, derive_new::new, Deserialize, Serialize)]
pub struct Message {
	pub destination: String,
	pub message: String,
}

pub async fn run(config: crate::config::AppConfig, bot_token: String) -> Result<()> {
	let addr = format!("127.0.0.1:{}", config.localhost_port);
	let listener = TcpListener::bind(&addr).await?;
	println!("Listening on: {}", addr);

	loop {
		let (mut socket, _) = listener.accept().await?;

		tokio::spawn(async move {
			let mut buf = [0; 1024];

			loop {
				let n = match socket.read(&mut buf).await {
					Ok(0) => return,
					Ok(n) => n,
					Err(e) => {
						eprintln!("Failed to read from socket: {}", e);
						return;
					}
				};

				let received = String::from_utf8_lossy(&buf[0..n]);
				match serde_json::from_str::<Message>(&received) {
					Ok(message) => {
						println!("Received: {:?}", message);

						if let Err(e) = socket.write_all(b"200\n").await {
							eprintln!("Failed to send acknowledgment: {}", e);
							return;
						}
					}
					Err(e) => {
						println!("Received invalid format: {}", received);
						eprintln!("Error parsing message: {}", e);

						if let Err(e) = socket.write_all(b"ERR: Invalid format\n").await {
							eprintln!("Failed to send error message: {}", e);
							return;
						}
					}
				}
			}
		});
	}
}
