use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use tokio::net::TcpStream;
use tracing::{info, warn};

/// Telegram DC2 address (149.154.167.50:443).
/// We check Telegram specifically, not generic internet — DNS-free, TLS-free, just TCP SYN/ACK.
const TELEGRAM_DC2: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(149, 154, 167, 50)), 443);

/// Check if Telegram is reachable via TCP connect to DC2.
pub async fn check_telegram_reachable() -> bool {
	tokio::time::timeout(std::time::Duration::from_secs(5), TcpStream::connect(TELEGRAM_DC2))
		.await
		.is_ok_and(|r| r.is_ok())
}

/// Block until Telegram is reachable, polling every second.
/// Logs on first failure and on recovery.
pub async fn wait_for_telegram() {
	if check_telegram_reachable().await {
		return;
	}

	warn!("Telegram not reachable, waiting for connectivity...");
	loop {
		tokio::time::sleep(std::time::Duration::from_secs(1)).await;
		if check_telegram_reachable().await {
			info!("Telegram is now reachable");
			return;
		}
	}
}
