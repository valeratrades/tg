use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use tokio::net::TcpStream;

/// Telegram DC2 address (149.154.167.50:443).
/// We check Telegram specifically, not generic internet — DNS-free, TLS-free, just TCP SYN/ACK.
const TELEGRAM_DC2: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(149, 154, 167, 50)), 443);

/// Check if Telegram is reachable via TCP connect to DC2.
pub async fn check_telegram_reachable() -> bool {
	tokio::time::timeout(std::time::Duration::from_secs(5), TcpStream::connect(TELEGRAM_DC2))
		.await
		.is_ok_and(|r| r.is_ok())
}
