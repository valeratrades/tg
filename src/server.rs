use std::{
	io::{Read, Seek, SeekFrom, Write},
	path::PathBuf,
	pin::{Pin, pin},
	sync::{Arc, OnceLock},
};

use eyre::Result;
use futures::future::{Either, select};
use futures_util::{StreamExt as _, stream::FuturesUnordered};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tg::telegram_chat_id;
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::{TcpListener, TcpStream},
	sync::{mpsc, oneshot},
	time::Interval,
};
use tracing::{debug, error, info, warn};
use v_utils::trades::Timeframe;
use xattr::FileExt as _;

use crate::{
	config::{LiveSettings, TopicsMetadata},
	pull::{self, topic_filepath},
};

/// A push request sent through the channel to the MTProto worker
struct PushRequest {
	updates: Vec<crate::sync::MessageUpdate>,
	response_tx: oneshot::Sender<Result<crate::sync::PushResults>>,
}

/// Handle to send push requests to the MTProto worker
#[derive(Clone)]
pub struct PushHandle {
	tx: mpsc::Sender<PushRequest>,
}

impl PushHandle {
	/// Send a push request and wait for the result
	pub async fn push(&self, updates: Vec<crate::sync::MessageUpdate>) -> Result<crate::sync::PushResults> {
		let (response_tx, response_rx) = oneshot::channel();
		let request = PushRequest { updates, response_tx };

		self.tx.send(request).await.map_err(|_| eyre::eyre!("Push worker channel closed"))?;

		response_rx.await.map_err(|_| eyre::eyre!("Push worker dropped response channel"))?
	}
}

pub static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Message to send to a specific topic in a forum group
#[derive(Clone, Debug, Default, Deserialize, Serialize, derive_new::new)]
pub struct Message {
	pub group_id: u64,
	pub topic_id: u64,
	pub message: String,
}

/// Request types that the server can handle
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum ServerRequest {
	/// Send a new message (legacy format, also supports bare Message object)
	Send(Message),
	/// Push updates (delete/edit/create) to Telegram via MTProto
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
			error: None,
			version: Some(env!("CARGO_PKG_VERSION").to_string()),
			push_results: None,
		}
	}

	pub fn ok_with_results(results: crate::sync::PushResults) -> Self {
		Self {
			success: true,
			error: None,
			version: Some(env!("CARGO_PKG_VERSION").to_string()),
			push_results: Some(results),
		}
	}

	pub fn err(msg: impl Into<String>) -> Self {
		Self {
			success: false,
			error: Some(msg.into()),
			version: Some(env!("CARGO_PKG_VERSION").to_string()),
			push_results: None,
		}
	}
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
pub async fn run(settings: Arc<LiveSettings>, bot_token: String, pull_interval: Timeframe) -> Result<()> {
	info!("Starting telegram server");

	// Discover forum topics and create topic files at startup
	info!("Discovering forum topics for configured groups...");
	pull::discover_and_create_topic_files(&settings, &bot_token).await?;

	// Create push channel and spawn the MTProto worker
	// This ensures all push operations are serialized through a single MTProto client
	let (push_tx, push_rx) = mpsc::channel::<PushRequest>(32);
	let push_handle = PushHandle { tx: push_tx };

	// Spawn the push worker task
	let worker_settings = Arc::clone(&settings);
	let worker_token = bot_token.clone();
	let push_worker = tokio::spawn(async move {
		run_push_worker(push_rx, &worker_settings, &worker_token).await;
	});

	let addr = format!("127.0.0.1:{}", settings.config()?.localhost_port);
	debug!("Binding to address: {}", addr);
	let listener = TcpListener::bind(&addr).await?;
	info!("Listening on: {}", addr);

	let interval_duration = pull_interval.duration();
	info!("Pull interval: {}", pull_interval);

	type BoxFut = Pin<Box<dyn std::future::Future<Output = (TaskResult, Arc<LiveSettings>, String, PushHandle)> + Send>>;
	let mut futures: FuturesUnordered<BoxFut> = FuturesUnordered::new();

	// Initial pull task
	let pull_interval_timer = tokio::time::interval(interval_duration);
	let settings_clone = Arc::clone(&settings);
	let token_clone = bot_token.clone();
	let push_handle_clone = push_handle.clone();
	futures.push(Box::pin(async move {
		let mut interval = pull_interval_timer;
		interval.tick().await;

		match pull::pull(&settings_clone, &token_clone).await {
			Ok(()) => debug!("Pull completed successfully"),
			Err(e) => warn!("Pull failed: {}", e),
		}

		(TaskResult::PullDone(interval), settings_clone, token_clone, push_handle_clone)
	}));

	loop {
		enum Event {
			NewConnection(std::io::Result<(TcpStream, std::net::SocketAddr)>),
			TaskCompleted(Option<(TaskResult, Arc<LiveSettings>, String, PushHandle)>),
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

				let settings_clone = Arc::clone(&settings);
				let token_clone = bot_token.clone();
				let push_handle_clone = push_handle.clone();
				futures.push(Box::pin(async move {
					handle_connection(socket, &settings_clone, &token_clone, &push_handle_clone).await;
					(TaskResult::ConnectionDone, settings_clone, token_clone, push_handle_clone)
				}));
			}
			Event::TaskCompleted(Some((result, settings_ret, token_ret, push_handle_ret))) => match result {
				TaskResult::ConnectionDone => {
					debug!("Connection task completed");
				}
				TaskResult::PullDone(mut interval) => {
					debug!("Pull task completed, re-queuing");
					let settings_clone = settings_ret;
					let token_clone = token_ret;
					let push_handle_clone = push_handle_ret;
					futures.push(Box::pin(async move {
						interval.tick().await;

						match pull::pull(&settings_clone, &token_clone).await {
							Ok(()) => debug!("Pull completed successfully"),
							Err(e) => warn!("Pull failed: {}", e),
						}

						(TaskResult::PullDone(interval), settings_clone, token_clone, push_handle_clone)
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

	// Clean up push worker
	push_worker.abort();

	info!("Server stopped");
	Ok(())
}

/// Worker task that owns the MTProto client and processes push requests sequentially
async fn run_push_worker(mut rx: mpsc::Receiver<PushRequest>, settings: &LiveSettings, bot_token: &str) {
	info!("Push worker started");

	while let Some(request) = rx.recv().await {
		debug!("Push worker processing request with {} updates", request.updates.len());

		let result = crate::sync::push(request.updates, settings, bot_token).await;

		// Send result back, ignore errors if receiver dropped
		let _ = request.response_tx.send(result);
	}

	info!("Push worker stopped (channel closed)");
}

async fn handle_connection(mut socket: TcpStream, _settings: &LiveSettings, bot_token: &str, push_handle: &PushHandle) {
	// Use a larger buffer for push requests which can contain many updates
	let mut buf = vec![0; 64 * 1024];

	while let Ok(n) = socket.read(&mut buf).await {
		if n == 0 {
			debug!("Connection closed by client");
			return;
		}

		let received = String::from_utf8_lossy(&buf[0..n]);
		debug!("Received raw message: {}", received);

		// Try parsing as ServerRequest first (has "type" field), fall back to legacy Message
		let request: ServerRequest = if let Ok(req) = serde_json::from_str(&received) {
			req
		} else if let Ok(msg) = serde_json::from_str::<Message>(&received) {
			// Legacy format: bare Message object
			ServerRequest::Send(msg)
		} else {
			error!("Failed to deserialize request. Raw: {}", received);
			let response = ServerResponse::err("Invalid request format");
			let _ = socket.write_all(serde_json::to_string(&response).unwrap().as_bytes()).await;
			return;
		};

		match request {
			ServerRequest::Send(message) => {
				// Handle send request (legacy behavior)
				handle_send_message(&mut socket, message, bot_token).await;
			}
			ServerRequest::Push { updates } => {
				// Handle push request via the serialized push worker
				handle_push_updates(&mut socket, updates, push_handle).await;
			}
			ServerRequest::Ping => {
				// Just respond with version info
				let response = ServerResponse::ok();
				let response_json = serde_json::to_string(&response).unwrap();
				if let Err(e) = socket.write_all(response_json.as_bytes()).await {
					error!("Failed to send ping response: {}", e);
				}
			}
		}
	}
}

/// Handle a push request by sending it to the push worker
async fn handle_push_updates(socket: &mut TcpStream, updates: Vec<crate::sync::MessageUpdate>, push_handle: &PushHandle) {
	info!("Received push request with {} updates", updates.len());

	let result = push_handle.push(updates).await;

	let response = match result {
		Ok(push_results) => {
			info!("Push completed successfully");
			ServerResponse::ok_with_results(push_results)
		}
		Err(e) => {
			error!("Push failed: {}", e);
			ServerResponse::err(e.to_string())
		}
	};

	let response_json = serde_json::to_string(&response).unwrap();
	if let Err(e) = socket.write_all(response_json.as_bytes()).await {
		error!("Failed to send push response: {}", e);
	}
}

/// Handle a send message request
async fn handle_send_message(socket: &mut TcpStream, message: Message, bot_token: &str) {
	// Send proper JSON response with version info
	let response = ServerResponse::ok();
	let response_json = serde_json::to_string(&response).unwrap();
	if let Err(e) = socket.write_all(response_json.as_bytes()).await {
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
		.and_then(|s| s.parse::<Timestamp>().inspect_err(|e| warn!("Failed to parse last_changed xattr: {}", e)).ok());

	let message_append_repr = format_message_append(&message.message, last_write_datetime, Timestamp::now());
	debug!("Formatted message append: {:?}", message_append_repr);

	// Ensure the parent directory exists
	if let Some(parent) = chat_filepath.parent()
		&& let Err(e) = std::fs::create_dir_all(parent)
	{
		error!("Failed to create directory '{}': {}", parent.display(), e);
		return;
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

	let now_rfc3339 = Timestamp::now().to_string();
	if let Err(e) = file.set_xattr("user.last_changed", now_rfc3339.as_bytes()) {
		error!("Failed to set xattr: {}", e);
		return;
	}
	debug!("Set xattr successfully");

	// Spawn background task to send to Telegram (with retries)
	// The message is written to file immediately without a tag.
	// When the send succeeds, the next sync will pull the message from TG (with its real ID)
	// and the tagless local message will be cleaned up.
	let message_clone = message.clone();
	let bot_token = bot_token.to_string();
	tokio::spawn(async move {
		let mut delay_secs = 1.0_f64;
		const E: f64 = std::f64::consts::E;
		const THIRTY_MINS_SECS: f64 = 30.0 * 60.0;

		loop {
			match send_message(&message_clone, &bot_token).await {
				Ok(msg_id) => {
					info!("Message sent successfully to Telegram with id {}", msg_id);
					// No need to update local file - sync will handle it
					break;
				}
				Err(e) => {
					let total_elapsed_secs = (delay_secs - 1.0) * (E - 1.0).recip() * E;
					if total_elapsed_secs >= THIRTY_MINS_SECS {
						error!("Failed to send message to Telegram after 30+ minutes: {}", e);
						// Give up - the tagless message will be cleaned up on next sync
						break;
					}
					warn!("Failed to send message to Telegram, retrying in {:.0}s: {}", delay_secs, e);
					tokio::time::sleep(std::time::Duration::from_secs_f64(delay_secs)).await;
					delay_secs *= E;
				}
			}
		}
	});
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

			let res: reqwest::Response = client.post(&url).multipart(form).send().await?;
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
	let res: reqwest::Response = client.post(&url).form(&params).send().await?;

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
pub fn format_message_append(message: &str, last_write_datetime: Option<Timestamp>, now: Timestamp) -> String {
	format_message_append_with_id(message, last_write_datetime, now, None)
}

/// Format a message append with an optional message ID marker
pub fn format_message_append_with_id(message: &str, last_write_datetime: Option<Timestamp>, now: Timestamp, msg_id: Option<i32>) -> String {
	format_message_append_with_sender(message, last_write_datetime, now, msg_id, None)
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

/// Format a message append with message ID and sender info
pub fn format_message_append_with_sender(message: &str, last_write_datetime: Option<Timestamp>, now: Timestamp, msg_id: Option<i32>, sender: Option<&str>) -> String {
	debug!("Formatting message append");

	let id_suffix = match (msg_id, sender) {
		(Some(id), Some(s)) => format!(" <!-- msg:{} {} -->", id, s),
		(Some(id), None) => format!(" <!-- msg:{} -->", id),
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
		format!("{}md\n{}\n{}\n{}", fence, message, fence, id_suffix.trim_start())
	} else if needs_dot_prefix {
		format!(". {}{}", message, id_suffix)
	} else {
		format!("{}{}", message, id_suffix)
	};

	// Assemble final output
	let prefix = if date_header.is_some() || needs_dot_prefix { "\n" } else { "" };
	format!("{}{}{}\n", date_header.unwrap_or_default(), prefix, formatted)
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
			let m = format_message_append(m, last_change, *t);
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
	fn deser_message() -> Result<()> {
		let message_str = r#"{"group_id":2244305221,"topic_id":3,"message":"a message"}"#;
		let message: Message = serde_json::from_str(message_str)?;
		insta::assert_debug_snapshot!(message, @r#"
		Message {
		    group_id: 2244305221,
		    topic_id: 3,
		    message: "a message",
		}
		"#);
		Ok(())
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
		<!-- msg:123 user -->
		");
	}

	#[test]
	fn test_simple_message_not_wrapped() {
		// Simple message without special patterns should NOT be wrapped
		let message = "just a simple message";

		let now = Timestamp::UNIX_EPOCH;
		let formatted = format_message_append_with_sender(message, None, now, Some(456), Some("bot"));

		insta::assert_snapshot!(formatted, @"just a simple message <!-- msg:456 bot -->");
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
		assert!(fence_line.chars().all(|c| c == '`'), "Closing fence should contain only backticks, got: {:?}", fence_line);

		// The msg tag should be on the NEXT line
		let tag_line = lines[fence_line_idx + 1];
		assert!(tag_line.contains("<!-- msg:999 bot -->"), "Tag should be on line after closing fence, got: {:?}", tag_line);
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
