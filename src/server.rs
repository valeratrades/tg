use std::{
	path::PathBuf,
	sync::{Arc, OnceLock},
};

use eyre::Result;
use futures_util::{StreamExt as _, stream::FuturesUnordered};
use grammers_client::Client;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::{TcpListener, TcpStream},
	sync::{mpsc, oneshot},
};
use tracing::{debug, error, info, warn};

use crate::{config::LiveSettings, pull};

/// Handle to send push requests to the MTProto worker
#[derive(Clone)]
pub struct PushHandle {
	tx: mpsc::Sender<PushRequest>,
}

impl PushHandle {
	pub async fn push(&self, updates: Vec<crate::sync::MessageUpdate>) -> Result<crate::sync::PushResults> {
		let (response_tx, response_rx) = oneshot::channel();
		self.tx.send(PushRequest { updates, response_tx }).await.map_err(|_| eyre::eyre!("Push channel closed"))?;
		response_rx.await.map_err(|_| eyre::eyre!("Push response channel closed"))?
	}
}

pub static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
/// Request types that the server can handle
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum ServerRequest {
	/// Push updates (create/delete/edit) to Telegram via MTProto
	Push { updates: Vec<crate::sync::MessageUpdate> },
	/// Ping to check server version
	Ping,
}
/// Response from the server
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ServerResponse {
	pub success: bool,
	pub error: Option<String>,
	/// Server version (CARGO_PKG_VERSION), included in all responses for client-side validation
	#[serde(default)]
	pub version: Option<String>,
	/// Detailed results from push operations
	#[serde(default)]
	pub push_results: Option<crate::sync::PushResults>,
}

impl ServerResponse {
	pub fn ok() -> Self {
		Self {
			success: true,
			version: Some(env!("CARGO_PKG_VERSION").to_string()),
			..Default::default()
		}
	}

	pub fn err(message: impl Into<String>) -> Self {
		Self {
			success: false,
			error: Some(message.into()),
			version: Some(env!("CARGO_PKG_VERSION").to_string()),
			..Default::default()
		}
	}

	pub fn ok_with_results(push_results: crate::sync::PushResults) -> Self {
		Self {
			success: true,
			version: Some(env!("CARGO_PKG_VERSION").to_string()),
			push_results: Some(push_results),
			..Default::default()
		}
	}
}

/// # Panics
/// On unsuccessful io operations
pub async fn run(settings: Arc<LiveSettings>) -> Result<()> {
	info!("Starting telegram server v{}", env!("CARGO_PKG_VERSION"));

	// Clear alerts state from previous session
	if let Err(e) = crate::alerts::AlertsState::clear() {
		warn!("Failed to clear alerts state: {e}");
	}

	// Bind TCP listener once — survives reconnections
	let addr = format!("127.0.0.1:{}", settings.config()?.localhost_port);
	debug!("Binding to address: {addr}");
	let listener = TcpListener::bind(&addr).await?;
	info!("Listening on: {addr}");

	//LOOP: main logic
	// Bounded exponential backoff: grammers reconnects its dead pool lazily with zero delay,
	// so a peer-FIN that survives TCP reachability checks (DC2 stays routable while the session
	// handshake fails) would otherwise spin connect→EOF→die at full speed. The backoff is the
	// real throttle; wait_for_telegram_draining only gates the truly-unroutable case.
	let mut backoff = std::time::Duration::from_secs(1);
	const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);
	loop {
		// Wait for Telegram connectivity while rejecting queued connections
		wait_for_telegram_draining(&listener).await;

		let session = match crate::mtproto::create_session(&settings) {
			Ok(s) => s,
			Err(e) => {
				warn!("Failed to create MTProto session: {e}; retrying in {backoff:?}");
				tokio::time::sleep(backoff).await;
				backoff = (backoff * 2).min(MAX_BACKOFF);
				continue;
			}
		};

		let started = std::time::Instant::now();
		match run_with_session(settings.clone(), session, &listener).await {
			Ok(()) => return Ok(()), // clean shutdown
			Err(e) => {
				warn!("Session died: {e}; reconnecting in {backoff:?}");
				tokio::time::sleep(backoff).await;
				// Reset only if the session stayed up long enough to count as healthy.
				backoff = if started.elapsed() >= MAX_BACKOFF {
					std::time::Duration::from_secs(1)
				} else {
					(backoff * 2).min(MAX_BACKOFF)
				};
			}
		}
	}
}
/// Format a message append with message ID and sender info
pub fn format_message_append_with_sender(message: &str, last_write_datetime: Option<Timestamp>, now: Timestamp, msg_id: Option<i32>, sender: Option<&str>) -> String {
	format_message_append(message, last_write_datetime, now, msg_id, sender, false, None)
}
/// Wait for Telegram connectivity while draining and rejecting any queued TCP connections.
/// This prevents CLI clients from hanging when the server is reconnecting.
async fn wait_for_telegram_draining(listener: &TcpListener) {
	if crate::connectivity::check_telegram_reachable().await {
		return;
	}

	warn!("Telegram not reachable, waiting for connectivity...");

	type ConnCheck = std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>;
	let mut connectivity: FuturesUnordered<ConnCheck> = FuturesUnordered::new();

	// Seed the initial connectivity check (with delay)
	connectivity.push(Box::pin(async {
		tokio::time::sleep(std::time::Duration::from_secs(1)).await;
		crate::connectivity::check_telegram_reachable().await
	}));

	// select! returns false while waiting, true once reachable.
	// Accept branch always returns false (drain and continue).
	// Connectivity branch returns the check result — true breaks the loop.
	while !tokio::select! {
		accept_result = listener.accept() => {
			if let Ok((mut socket, addr)) = accept_result {
				warn!("Rejecting connection from {addr}: server is reconnecting to Telegram");
				let response = ServerResponse::err("Server is reconnecting to Telegram, try again shortly");
				let _ = socket.write_all(serde_json::to_string(&response).unwrap().as_bytes()).await;
			}
			false
		}
		Some(reachable) = connectivity.next() => {
			if !reachable {
				connectivity.push(Box::pin(async {
					tokio::time::sleep(std::time::Duration::from_secs(1)).await;
					crate::connectivity::check_telegram_reachable().await
				}));
			}
			reachable
		}
	} {}

	info!("Telegram is now reachable");
}

/// Run the server with an established session. Returns when a fatal error occurs or on clean shutdown.
///
/// `worker_loop` owns all MTProto + file-mutating work (pull, alerts, pushes) and runs it serially,
/// so concurrent writes can never corrupt a topic file. `network_loop` only accepts connections and
/// forwards their requests to the worker — it must never block on the worker, otherwise a slow pull
/// would stall the ack and clients would time out (the bug this split fixes).
async fn run_with_session(settings: Arc<LiveSettings>, session: crate::mtproto::MtprotoSession, listener: &TcpListener) -> Result<()> {
	let crate::mtproto::MtprotoSession { client, runner } = session;

	let (push_tx, push_rx) = mpsc::channel::<PushRequest>(32);
	let push_handle = PushHandle { tx: push_tx };

	tokio::select! {
		biased;
		result = worker_loop(&settings, &client, push_rx) => { client.disconnect(); result }
		result = network_loop(listener, &push_handle) => { client.disconnect(); result }
		_ = runner.run() => Err(eyre::eyre!("MTProto runner exited")),
	}
}
/// Serially process all MTProto + file-mutating work. Runs concurrently with the runner, which
/// drives the I/O the RPC-dependent setup needs. Any error here is fatal: it tears the session
/// down so `run` can reconnect.
async fn worker_loop(settings: &LiveSettings, client: &Client, mut push_rx: mpsc::Receiver<PushRequest>) -> Result<()> {
	if !client.is_authorized().await? {
		eyre::bail!("Session not authorized. Run `tg pull` interactively first to authenticate.");
	}
	info!("Telegram client authorized (existing session)");

	info!("Discovering forum topics for configured groups...");
	pull::discover_and_create_topic_files(settings, client).await?;

	let cfg = settings.config()?;
	info!("Pull interval: {}", cfg.pull_interval);
	// `Delay` (≥interval between ticks regardless of backlog) prevents the catch-up storm `Burst`
	// would cause once work errors in µs.
	let mut pull_timer = tokio::time::interval(cfg.pull_interval.duration());
	pull_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	let mut alerts_timer = tokio::time::interval(cfg.alerts_interval.duration());
	alerts_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

	//LOOP: serial worker
	loop {
		tokio::select! {
			biased;
			// On-demand pushes win over periodic ticks so interactive sends aren't delayed.
			// ponytail: a continuous push flood could starve pull; not a concern for a personal CLI.
			Some(req) = push_rx.recv() => {
				let result = crate::sync::push(req.updates, settings, client).await;
				let _ = req.response_tx.send(result);
			}
			_ = pull_timer.tick() => {
				pull::pull(settings, client).await?; // fatal connection-dead → tears session down
				debug!("Pull completed successfully");
			}
			_ = alerts_timer.tick() => {
				crate::alerts::check_alerts(client, settings).await?;
				debug!("Alerts check completed successfully");
			}
		}
	}
}
/// Accept connections and run each request handler concurrently. Connection-level failures are
/// isolated per-connection; only a listener failure is fatal.
async fn network_loop(listener: &TcpListener, push_handle: &PushHandle) -> Result<()> {
	type BoxFut = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
	let mut conns: FuturesUnordered<BoxFut> = FuturesUnordered::new();

	loop {
		tokio::select! {
			accept_result = listener.accept() => {
				let (socket, addr) = accept_result?;
				info!("Accepted connection from: {addr}");
				conns.push(Box::pin(handle_connection(socket, push_handle.clone())));
			}
			Some(()) = conns.next() => {} // reap finished connection handlers
		}
	}
}
/// Send a message (text, or a photo when it's a `![](path)` image) via MTProto.
/// Returns the message ID assigned by Telegram.
pub(crate) async fn send_message(client: &Client, group_id: u64, topic_id: u64, text: &str) -> Result<i32> {
	if let Some((image_path, caption)) = extract_image_path(text) {
		let full_path = DATA_DIR.get().unwrap().join(&image_path);
		if full_path.exists() {
			debug!("Sending photo from {}", full_path.display());
			return crate::mtproto::send_photo(client, group_id, topic_id, &full_path, caption.as_deref()).await;
		}
		warn!("Image file not found: {}, sending as text", full_path.display());
	}
	crate::mtproto::send_text_message(client, group_id, topic_id, text).await
}

pub(crate) fn format_message_append(
	message: &str,
	last_write_datetime: Option<Timestamp>,
	now: Timestamp,
	msg_id: Option<i32>,
	sender: Option<&str>,
	forwarded: bool,
	reply_to_msg_id: Option<i32>,
) -> String {
	debug!("Formatting message append");

	let fwd_prefix = if forwarded { "forwarded " } else { "" };
	let ts_part = format!(" ts:{}", now.as_second());
	let reply_part = reply_to_msg_id.map(|id| format!(" reply_to:{id}")).unwrap_or_default();
	let id_suffix = match (msg_id, sender) {
		(Some(id), Some(s)) => format!(" <!-- {fwd_prefix}msg:{id}{ts_part}{reply_part} {s} -->"),
		(Some(id), None) => format!(" <!-- {fwd_prefix}msg:{id}{ts_part}{reply_part} -->"),
		_ => String::new(),
	};

	// Determine if we need a time gap separator or date header
	let (date_header, needs_dot_prefix) = if let Some(last_write_datetime) = last_write_datetime {
		let duration = now.duration_since(last_write_datetime);
		let six_minutes = jiff::SignedDuration::from_mins(6);

		if duration >= six_minutes {
			let now_date = now.to_zoned(jiff::tz::TimeZone::UTC).date();
			let last_date = last_write_datetime.to_zoned(jiff::tz::TimeZone::UTC).date();
			if now_date == last_date {
				(None, true)
			} else {
				(Some(format!("\n## {}\n", now_date.strftime("%b %d, %Y"))), false)
			}
		} else {
			(None, false)
		}
	} else {
		(None, false)
	};

	// Wrap in code block if message has patterns that break our format
	// Use enough backticks to avoid collision with code inside the message (min 5)
	// No need for ". " prefix when wrapped - the code block itself separates
	// Tag goes on its own line after the closing fence
	let formatted = if needs_markdown_wrapping(message) {
		let fence_len = max_backtick_run(message).max(4) + 1; // at least 5, or 1 more than content
		let fence = "`".repeat(fence_len);
		// Closing fence must be on its own line (CommonMark spec), tag goes on next line
		format!("{fence}md\n{message}\n{fence}\n{}", id_suffix.trim_start())
	} else if needs_dot_prefix {
		format!(". {message}{id_suffix}")
	} else {
		format!("{message}{id_suffix}")
	};

	// Assemble final output
	let prefix = if date_header.is_some() || needs_dot_prefix { "\n" } else { "" };
	format!("{}{prefix}{formatted}\n", date_header.unwrap_or_default())
}
/// A push request sent through the channel to the MTProto worker
struct PushRequest {
	updates: Vec<crate::sync::MessageUpdate>,
	response_tx: oneshot::Sender<Result<crate::sync::PushResults>>,
}
/// Read one request from a client, forward it to the serial worker, and write back the result.
/// Runs concurrently with other connections; the worker it calls into is what serializes the work.
async fn handle_connection(mut socket: TcpStream, push_handle: PushHandle) {
	// Larger buffer for push requests which can contain many updates
	let mut buf = vec![0; 64 * 1024];

	while let Ok(n) = socket.read(&mut buf).await {
		if n == 0 {
			debug!("Connection closed by client");
			return;
		}

		let received = String::from_utf8_lossy(&buf[0..n]);
		debug!("Received raw message: {received}");

		let response = match serde_json::from_str::<ServerRequest>(&received) {
			Ok(ServerRequest::Push { updates }) => {
				info!("Received push request with {} updates", updates.len());
				match push_handle.push(updates).await {
					Ok(results) => {
						info!("Push completed successfully");
						ServerResponse::ok_with_results(results)
					}
					Err(e) => {
						error!("Push failed: {e}");
						ServerResponse::err(e.to_string())
					}
				}
			}
			Ok(ServerRequest::Ping) => ServerResponse::ok(),
			Err(e) => {
				error!("Failed to deserialize request: {e}. Raw: {received}");
				ServerResponse::err("Invalid request format")
			}
		};

		if let Err(e) = socket.write_all(serde_json::to_string(&response).unwrap().as_bytes()).await {
			error!("Failed to send response: {e}");
		}
	}
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
/// Check if a message contains patterns that could break our markdown format
fn needs_markdown_wrapping(message: &str) -> bool {
	// Patterns that could break our format:
	// - Double newlines (paragraph breaks)
	// - Lines starting with # (headers)
	// - Lines starting with ## (our date headers)
	// - Lines starting with . followed by space (our message separator)
	// - Backticks (``` or more) that could interfere with code blocks
	message.contains("\n\n")
		|| message.contains("```")
		|| message.lines().any(|line| {
			let trimmed = line.trim();
			trimmed.starts_with('#') || trimmed.starts_with(". ")
		})
}
/// Find the longest sequence of backticks in a message
fn max_backtick_run(message: &str) -> usize {
	let mut max = 0;
	let mut current = 0;
	for c in message.chars() {
		if c == '`' {
			current += 1;
			max = max.max(current);
		} else {
			current = 0;
		}
	}
	max
}
#[cfg(test)]
mod tests {
	use v_utils::distributions::ReimanZeta;

	use super::*;

	#[test]
	fn test_format_message_append() {
		let mut messages = Vec::new();

		let zeta_dist = ReimanZeta::new(1.0, 4320);

		let mut accumulated_offset = Timestamp::UNIX_EPOCH;
		for i in 0..10 {
			let time_offset_mins = zeta_dist.sample(Some(i));
			let time_offset = jiff::Span::new().minutes(time_offset_mins as i64);
			accumulated_offset = accumulated_offset.checked_add(time_offset).unwrap();
			let message = accumulated_offset.strftime("%Y-%m-%d %H:%M:%S").to_string(); // for ease of testing

			messages.push((message, accumulated_offset));
		}

		messages.sort_by_key(|&(_, timestamp)| timestamp);

		let mut formatted_messages = String::new();
		for (i, (m, t)) in messages.iter().enumerate() {
			let last_change = if i == 0 { None } else { Some(messages[i - 1].1) };
			let m = format_message_append_with_sender(m, last_change, *t, None, None);
			formatted_messages.push_str(&m);
		}

		insta::assert_snapshot!(formatted_messages, @"
		1970-01-01 06:30:00

		## Jan 03, 1970

		1970-01-03 15:41:00

		. 1970-01-03 15:49:00
		1970-01-03 15:50:00

		. 1970-01-03 16:56:00

		. 1970-01-03 17:08:00

		. 1970-01-03 17:19:00
		1970-01-03 17:20:00

		. 1970-01-03 17:33:00

		. 1970-01-03 20:36:00
		");
	}

	#[test]
	fn test_multiline_message_with_special_patterns() {
		// This message has patterns that would break our format:
		// - Double newline (paragraph break)
		// - Line starting with # (header)
		let message = "TODO: integrate resume into the site\n\n# Impl details\nwhile at it, add photo and general info on yourself at the top of /contact\n\n// will no longer need separate repo for it";

		let now = Timestamp::UNIX_EPOCH;
		let formatted = format_message_append_with_sender(message, None, now, Some(123), Some("user"));

		// Should be wrapped in ```md block
		insta::assert_snapshot!(formatted, @"
		`````md
		TODO: integrate resume into the site

		# Impl details
		while at it, add photo and general info on yourself at the top of /contact

		// will no longer need separate repo for it
		`````
		<!-- msg:123 ts:0 user -->
		");
	}

	#[test]
	fn test_simple_message_not_wrapped() {
		// Simple message without special patterns should NOT be wrapped
		let message = "just a simple message";

		let now = Timestamp::UNIX_EPOCH;
		let formatted = format_message_append_with_sender(message, None, now, Some(456), Some("bot"));

		insta::assert_snapshot!(formatted, @"just a simple message <!-- msg:456 ts:0 bot -->");
	}

	#[test]
	fn test_message_with_dot_prefix_wrapped() {
		// Message starting with ". " would be confused with our separator
		let message = ". this looks like a separator\nbut it's actually content";

		let now = Timestamp::UNIX_EPOCH;
		let formatted = format_message_append_with_sender(message, None, now, Some(789), Some("user"));

		// Should be wrapped
		assert!(formatted.contains("```md"));
		assert!(formatted.contains("```"));
	}

	#[test]
	fn test_code_block_closing_fence_is_valid_commonmark() {
		// Per CommonMark spec, a closing code fence must contain ONLY fence characters
		// (optionally followed by whitespace). Adding <!-- msg:xxx --> on the same line
		// breaks the fence, causing the code block to not close properly.
		//
		// This test ensures we produce valid CommonMark output.
		let message = "Some message\n\nWith paragraph break";

		let now = Timestamp::UNIX_EPOCH;
		let formatted = format_message_append_with_sender(message, None, now, Some(999), Some("bot"));

		// The closing fence line should be ONLY backticks (no comment on same line)
		let lines: Vec<&str> = formatted.lines().collect();

		// Find the closing fence line
		let fence_line_idx = lines.iter().position(|l| l.starts_with("`````") && !l.contains("md")).unwrap();
		let fence_line = lines[fence_line_idx];

		// The fence line should contain ONLY backticks (possibly with trailing newline already stripped)
		assert!(fence_line.chars().all(|c| c == '`'), "Closing fence should contain only backticks, got: {fence_line:?}");

		// The msg tag should be on the NEXT line
		let tag_line = lines[fence_line_idx + 1];
		assert!(
			tag_line.contains("<!-- msg:999") && tag_line.contains("bot -->"),
			"Tag should be on line after closing fence, got: {tag_line:?}"
		);
	}

	#[test]
	fn no_duplicate_date_header_with_stale_xattr() {
		// Regression test: when pull writes "## Feb 25, 2026" header + messages to file
		// but the xattr still has a timestamp from Feb 24, the send path would read the
		// stale xattr and emit another "## Feb 25, 2026" header → duplicate.
		//
		// After the fix, merge_mtproto_messages_to_file updates xattr, so the send path
		// sees the correct last_write_datetime and doesn't emit a duplicate.

		// Simulate: xattr says "Feb 24 at 22:00" (stale from previous session)
		let stale_xattr_time = jiff::civil::date(2026, 2, 24).at(22, 0, 0, 0).to_zoned(jiff::tz::TimeZone::UTC).unwrap().timestamp();
		// New message arrives on Feb 25 at 14:00
		let now = jiff::civil::date(2026, 2, 25).at(14, 0, 0, 0).to_zoned(jiff::tz::TimeZone::UTC).unwrap().timestamp();

		let formatted = format_message_append_with_sender("new message after restart", Some(stale_xattr_time), now, Some(3416), None);
		// With stale xattr pointing to Feb 24, this would emit "## Feb 25, 2026" header
		// even though the file already has that header from pull.
		assert!(formatted.contains("## Feb 25, 2026"), "stale xattr produces date header (pre-fix behavior)");

		// Now simulate the correct scenario: xattr updated by pull to reflect actual last message time
		let correct_xattr_time = jiff::civil::date(2026, 2, 25).at(12, 0, 0, 0).to_zoned(jiff::tz::TimeZone::UTC).unwrap().timestamp();
		let formatted = format_message_append_with_sender("new message after restart", Some(correct_xattr_time), now, Some(3416), None);
		// With correct xattr on the same day (2h gap > 6min), should get ". " prefix, NOT a new header
		assert!(!formatted.contains("## Feb 25"), "correct xattr should not produce duplicate header");
		assert!(formatted.contains(". new message"), "same-day gap should produce dot prefix");
	}

	#[test]
	fn test_message_containing_code_blocks_formats_correctly() {
		// Reproduce the issue from tooling.md - messages that themselves contain code blocks
		let message = r#"do note that milestones can easily consist of just one issue, so no problems there.

Q: what do we do for operating on multiple milestones though? Will need some naming standard wtih inverent sorting properties. But dates won't work out of the box, as some task-sets don't have them set in stone
A: wait, but we can have explicit (or even implicit) deadline move semantics, like existing `milestones` currently do"#;

		let now = Timestamp::UNIX_EPOCH;
		let formatted = format_message_append_with_sender(message, None, now, Some(2797), Some("bot"));

		// Should be wrapped due to double newlines
		assert!(formatted.contains("`````md"), "Should start with 5-backtick fence");

		// Verify proper CommonMark structure
		let lines: Vec<&str> = formatted.lines().collect();
		let closing_fence_idx = lines.iter().rposition(|l| l.starts_with("`````") && !l.contains("md")).unwrap();

		// Closing fence should be pure backticks
		assert!(
			lines[closing_fence_idx].chars().all(|c| c == '`'),
			"Closing fence must be only backticks: {:?}",
			lines[closing_fence_idx]
		);
	}
}
