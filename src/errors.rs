use std::fmt;

use miette::{Diagnostic, SourceSpan};
use reqwest::StatusCode;

/// Telegram Bot API error
#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum TelegramApiError {
	#[error("Telegram API authentication failed (HTTP {status})")]
	#[diagnostic(
		code(tg::telegram::unauthorized),
		help("The bot token is invalid or revoked.\nUpdate the token and restart: `systemctl --user restart tg-server`")
	)]
	Unauthorized { status: u16, body: String },

	#[error("Telegram API rejected the request (HTTP {status}): {body}")]
	#[diagnostic(code(tg::telegram::client_error))]
	ClientError { status: u16, body: String },

	#[error("Telegram API server error (HTTP {status}): {body}")]
	#[diagnostic(code(tg::telegram::server_error), help("Telegram servers may be temporarily unavailable. This is retryable."))]
	ServerError { status: u16, body: String },

	#[error("failed to reach Telegram API")]
	#[diagnostic(code(tg::telegram::network), help("Check your network connection."))]
	Network(#[source] reqwest::Error),

	#[error("Telegram API returned unparseable response")]
	#[diagnostic(code(tg::telegram::bad_response))]
	BadResponse(#[source] serde_json::Error),
}

impl TelegramApiError {
	pub fn from_status(status: StatusCode, body: String) -> Self {
		let code = status.as_u16();
		match code {
			401 | 403 => Self::Unauthorized { status: code, body },
			400..=499 => Self::ClientError { status: code, body },
			_ => Self::ServerError { status: code, body },
		}
	}

	/// Whether this error is fatal and the service should stop.
	pub fn is_fatal(&self) -> bool {
		matches!(self, Self::Unauthorized { .. })
	}

	/// Whether this error is transient and the operation should be retried.
	pub fn is_retryable(&self) -> bool {
		matches!(self, Self::ServerError { .. } | Self::Network(_))
	}
}

/// Error for JSON parsing failures with source highlighting
#[derive(Clone, Debug, Diagnostic)]
#[diagnostic(code(tg::json_parse), help("The server returned invalid JSON. This may indicate a server bug or version mismatch."))]
pub struct JsonParseError {
	#[source_code]
	pub src: String,
	#[label("parse error here")]
	pub span: SourceSpan,
	/// Store the error message since serde_json::Error doesn't implement Clone
	pub cause_msg: String,
}
impl JsonParseError {
	pub fn from_serde(src: String, err: serde_json::Error) -> Self {
		// serde_json provides line/column info
		let offset = if err.line() > 0 {
			// Calculate byte offset from line/column
			let mut offset = 0;
			for (i, line) in src.lines().enumerate() {
				if i + 1 == err.line() {
					offset += err.column().saturating_sub(1);
					break;
				}
				offset += line.len() + 1; // +1 for newline
			}
			offset
		} else {
			0
		};

		// Span length of 1 at the error position
		let span = (offset, 1).into();
		let cause_msg = err.to_string();

		Self { src, span, cause_msg }
	}
}

impl fmt::Display for JsonParseError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "Failed to parse JSON response: {}", self.cause_msg)
	}
}

impl std::error::Error for JsonParseError {}

/// Error for server connection failures
#[derive(Clone, Debug, Diagnostic)]
#[diagnostic(
	code(tg::connection),
	help("Either the server is not running, or it's running on a different port.\nStart/restart it with `systemctl --user restart tg-server` or `tg server`")
)]
pub struct ConnectionError {
	pub addr: String,
	/// Store the error message since std::io::Error doesn't implement Clone
	pub cause_msg: String,
}

impl ConnectionError {
	pub fn new(addr: String, cause: std::io::Error) -> Self {
		Self { addr, cause_msg: cause.to_string() }
	}
}

impl fmt::Display for ConnectionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "Cannot connect to tg server at {}: {}", self.addr, self.cause_msg)
	}
}

impl std::error::Error for ConnectionError {}

/// Error for server version mismatch
#[derive(Clone, Debug, Diagnostic)]
#[diagnostic(
	code(tg::version_mismatch),
	help("Restart the server to use the updated version:\n  systemctl --user restart tg-server\nor:\n  tg server")
)]
pub struct VersionMismatchError {
	pub server_version: String,
	pub client_version: String,
}

impl fmt::Display for VersionMismatchError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "Server version mismatch: server is v{}, client is v{}", self.server_version, self.client_version)
	}
}

impl std::error::Error for VersionMismatchError {}

/// Error when no topic matches the pattern
#[derive(Clone, Debug, Diagnostic)]
#[diagnostic(code(tg::topic_not_found), help("Use `tg list` to see available topics, or check your config for group definitions."))]
pub struct TopicNotFoundError {
	pub pattern: String,
}

impl fmt::Display for TopicNotFoundError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "No topic found matching pattern: {}", self.pattern)
	}
}

impl std::error::Error for TopicNotFoundError {}
