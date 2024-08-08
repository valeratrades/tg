use anyhow::Result;
use crate::config::AppConfig;
use tokio::net::TcpListener;
use tokio::io::AsyncReadExt;

// force having an active daemon
// send all messages to the daemon (localhost?)
// daemon accepts messages, updates the local knowledge, then immediately appends to target files
	// - iter over configured channels and groups, if any matches, push them all level down. So for my config it's: top-level: (alerts, 2839885895: (journal, making, trading, general))
// local knowledge diff is pushed to remote when internet access is available

//  currently impossible:
// - every time I'm rerequesting the currently available tg messages, we clean the message pipe cache

pub async fn run(config: AppConfig, bot_token: String) -> Result<()> {
	let addr = format!("127.0.0.1:{}", config.localhost_port);
	let listener = TcpListener::bind(&addr).await?;
	println!("Listening on: {}", addr);

	loop {
		let (mut socket, _) = listener.accept().await?;

		tokio::spawn(async move {
			let mut buf = [0; 1024];

			loop {
				let n = match socket.read(&mut buf).await {
					Ok(n) if n == 0 => return,
					Ok(n) => n,
					Err(e) => {
						eprintln!("Failed to read from socket: {}", e);
						return;
					}
				};

				let received = String::from_utf8_lossy(&buf[0..n]);
				if let Some((key, value)) = received.split_once(',') {
					println!("Received: ({}, {})", key.trim(), value.trim());
				} else {
					println!("Received invalid format: {}", received);
				}
			}
		});
	}
}
