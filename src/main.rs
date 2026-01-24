use std::{
	io::Write as IoWrite,
	process::{Command, Stdio},
};

use clap::{Args, Parser, Subcommand};
use clap_stdin::MaybeStdin;
use eyre::{Result, bail, eyre};
use jiff::{Timestamp, ToSpan, civil::Date};
use server::Message;
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::TcpStream,
};
use v_utils::{io::file_open::open, trades::Timeframe};

use crate::{
	config::{LiveSettings, SettingsFlags, TopicsMetadata},
	sync::PushResults,
};

pub mod config;
mod mtproto;
pub mod pull;
mod server;
mod shell_init;
mod sync;

#[derive(Clone, Debug, Parser)]
#[command(author, version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")"), about, long_about = None)]
pub struct Cli {
	#[command(subcommand)]
	command: Commands,
	#[command(flatten)]
	settings: SettingsFlags,
	#[arg(long)]
	token: Option<String>,
}

#[derive(Clone, Debug, Subcommand)]
enum Commands {
	/// Send a message to a channel/topic
	/// Ex:
	/// ```sh
	/// tg send -c journal "today I'm feeling blue"
	/// tg send -g 2244305221 -t 7 "direct message"
	/// echo "piped message" | tg send -c journal -
	/// ```
	Send(SendArgs),
	/// Get information about the bot
	BotInfo,
	/// Start a telegram server, syncing messages from configured forum groups
	Server(ServerArgs),
	/// Open a topic file with $EDITOR. Uses fzf for pattern matching.
	/// Ex:
	/// ```sh
	/// tg open           # fzf over all topics
	/// tg open journal   # open if unique, else fzf
	/// ```
	Open(OpenArgs),
	/// Pull messages from Telegram for all configured forum groups
	Pull(PullArgs),
	/// List all discovered topics
	List,
	/// Aggregate TODOs from all topics
	#[command(subcommand)]
	Todos(TodosCommands),
	/// Shell aliases and hooks. Usage: `tg init <shell> | source`
	Init(shell_init::ShellInitArgs),
	/// Directly schedule an update (delete or edit) for a specific message
	/// Ex:
	/// ```sh
	/// tg schedule-update delete 2244305221 1 2645
	/// tg schedule-update edit 2244305221 1 2645 "new message text"
	/// ```
	ScheduleUpdate(ScheduleUpdateArgs),
}

#[derive(Args, Clone, Debug)]
struct SendArgs {
	/// Pattern to match channel/topic name (uses fzf if multiple matches)
	#[arg(short, long)]
	channel: Option<String>,
	/// Direct group ID (bypasses pattern matching)
	#[arg(short, long)]
	group_id: Option<u64>,
	/// Direct topic ID (requires --group-id)
	#[arg(short, long)]
	topic_id: Option<u64>,
	/// Message to send. Pass '-' to read from stdin.
	message: MaybeStdin<String>,
}

#[derive(Args, Clone, Debug)]
struct OpenArgs {
	/// Pattern to match topic name (uses fzf if multiple matches)
	pattern: Option<String>,
}

#[derive(Args, Clone, Debug)]
struct PullArgs {
	/// Reset sync state and re-fetch all messages (clears topic files)
	/// Use this to add message ID markers to old messages for TODO tracking
	#[arg(long)]
	reset: bool,
}

#[derive(Args, Clone, Debug)]
struct ServerArgs {
	/// Interval for periodic pull from Telegram (e.g., "1m", "5m", "1h")
	#[arg(long, default_value = "1m")]
	pull_interval: Timeframe,
}

#[derive(Clone, Debug, Subcommand)]
enum TodosCommands {
	/// Compile TODOs from all topics into todos.md
	Compile,
	/// Compile TODOs and open todos.md with $EDITOR
	Open,
}

#[derive(Args, Clone, Debug)]
struct ScheduleUpdateArgs {
	/// Action to perform
	#[command(subcommand)]
	action: UpdateAction,
}

#[derive(Clone, Debug, Subcommand)]
enum UpdateAction {
	/// Delete a message from Telegram
	Delete {
		/// Group ID
		group_id: u64,
		/// Topic ID
		topic_id: u64,
		/// Message ID
		message_id: i32,
	},
	/// Edit a message on Telegram
	Edit {
		/// Group ID
		group_id: u64,
		/// Topic ID (required for local file cleanup)
		topic_id: u64,
		/// Message ID
		message_id: i32,
		/// New message content
		new_content: String,
	},
	/// Create (send) a new message to Telegram
	/// Note: This is handled by the server's background tasks; using this directly
	/// will write the message to file without a tag and queue it for sending.
	Create {
		/// Group ID
		group_id: u64,
		/// Topic ID
		topic_id: u64,
		/// Message content
		content: String,
	},
}

#[tokio::main]
async fn main() -> Result<()> {
	use std::{sync::Arc, time::Duration};

	v_utils::clientside!();
	let cli = Cli::parse();
	let settings = Arc::new(LiveSettings::new(cli.settings, Duration::from_secs(5)).expect("Failed to read config file"));
	config::init_data_dir();
	let bot_token = match cli.token {
		Some(t) => t,
		None => std::env::var("TELEGRAM_MAIN_BOT_TOKEN").expect("TELEGRAM_MAIN_BOT_TOKEN not set"),
	};

	match cli.command {
		Commands::Send(args) => {
			let (group_id, topic_id) = resolve_send_destination(&args)?;
			let msg_text = args.message.to_string();

			let message = Message::new(group_id, topic_id, msg_text.clone());
			let addr = format!("127.0.0.1:{}", settings.config()?.localhost_port);

			// Connect to server (required)
			let mut stream = TcpStream::connect(&addr).await.map_err(|_| eyre::eyre!("Server not running. Start it with `tg server`"))?;

			// Server handles send + file write with message tag
			let json = serde_json::to_string(&message)?;
			stream.write_all(json.as_bytes()).await?;

			// Read JSON response and check version
			let mut buf = vec![0u8; 4096];
			let n = stream.read(&mut buf).await?;
			if n == 0 {
				bail!("Server closed connection without response");
			}
			let response: server::ServerResponse = serde_json::from_slice(&buf[..n])?;
			sync::check_server_version(&response)?;
		}
		Commands::BotInfo => {
			let url = format!("https://api.telegram.org/bot{bot_token}/getMe");
			let client = reqwest::Client::new();
			let res: reqwest::Response = client.get(&url).send().await?;

			let parsed_json: serde_json::Value = serde_json::from_str(&res.text().await?).expect("Failed to parse JSON");
			let pretty_json = serde_json::to_string_pretty(&parsed_json).expect("Failed to pretty print JSON");
			println!("{pretty_json}");
		}
		Commands::Server(args) => {
			server::run(Arc::clone(&settings), bot_token, args.pull_interval).await?;
		}
		Commands::Pull(args) => {
			if args.reset {
				// Reset mode: clear sync timestamps and topic files, then re-pull everything
				eprintln!("WARNING: This will delete all local topic files and re-fetch from Telegram.");
				eprintln!("This is necessary to add message ID markers for TODO deletion tracking.");
				eprint!("Continue? [y/N] ");
				std::io::stdout().flush()?;
				let mut input = String::new();
				std::io::stdin().read_line(&mut input)?;
				if !input.trim().eq_ignore_ascii_case("y") {
					eprintln!("Aborted.");
					return Ok(());
				}

				// Clear topic files (sync state is now derived from file content)
				let data_dir = server::DATA_DIR.get().unwrap();
				let metadata = TopicsMetadata::load();
				for (group_id, group) in &metadata.groups {
					let default_name = format!("group_{group_id}");
					let group_name = group.name.as_deref().unwrap_or(&default_name);
					let group_dir = data_dir.join(group_name);
					if group_dir.exists() {
						for entry in std::fs::read_dir(&group_dir)? {
							let entry = entry?;
							let path = entry.path();
							if path.extension().map(|e| e == "md").unwrap_or(false) {
								std::fs::remove_file(&path)?;
								eprintln!("Removed {}", path.display());
							}
						}
					}
				}

				eprintln!("Re-pulling all messages...");
			}
			pull::pull(&settings, &bot_token).await?;
		}
		Commands::Open(args) => {
			let path = resolve_topic_path(args.pattern.as_deref())?;

			// Read file content before opening
			let old_content = std::fs::read_to_string(&path).unwrap_or_default();

			// Open with editor
			open(&path)?;

			// Read file content after closing editor
			let new_content = std::fs::read_to_string(&path).unwrap_or_default();

			// Detect changes including new messages to send
			let changes = sync::detect_changes_with_new_messages(&old_content, &new_content);

			// Warn about invalid inserts (content added before last known message)
			if changes.has_invalid_inserts() {
				eprintln!("Warning: Cannot send messages back in time. The following content was added before the last known message:");
				for line in &changes.invalid_inserts {
					eprintln!("  {line}");
				}
				eprintln!("These changes were not sent. Please add new messages at the end of the file.");
			}

			if !changes.is_empty() {
				if let Some((group_id, topic_id)) = sync::resolve_topic_ids_from_path(&path) {
					let updates = sync::changes_to_updates(&changes, group_id, topic_id);
					let results = sync::push_via_server(updates, &settings).await?;
					display_push_results(&results);
				} else {
					eprintln!("Warning: Could not resolve topic IDs from path, changes not synced");
				}
			}
		}
		Commands::List => {
			list_topics()?;
		}
		Commands::Todos(cmd) => match cmd {
			TodosCommands::Compile => {
				aggregate_todos(&settings)?;
			}
			TodosCommands::Open => {
				let path = aggregate_todos(&settings)?;

				// Read the generated todos.md to get the old state
				let old_content = std::fs::read_to_string(&path).unwrap_or_default();
				let old_todos = parse_todos_file(&old_content);

				// Count total TODOs vs trackable ones
				let total_todo_lines = old_content.lines().filter(|l| l.contains("<!-- todo:")).count();
				let trackable_count = old_todos.len();
				let untrackable_count = total_todo_lines - trackable_count;

				if trackable_count == 0 && total_todo_lines > 0 {
					eprintln!("Note: {total_todo_lines} TODOs found but none have message IDs (old messages).");
					eprintln!("Run `tg pull --reset` to add message IDs for sync.");
				} else if untrackable_count > 0 {
					eprintln!("Note: {trackable_count}/{total_todo_lines} TODOs trackable. {untrackable_count} need `tg pull --reset`.");
				}

				// Open with editor
				open(&path)?;

				// Read the file after editing
				let new_content = std::fs::read_to_string(&path).unwrap_or_default();
				let new_todos = parse_todos_file(&new_content);

				// Find deleted TODOs and convert to MessageUpdates
				let deleted: Vec<_> = old_todos.difference(&new_todos).cloned().collect();

				if !deleted.is_empty() {
					eprintln!("Deleting {} TODO(s):", deleted.len());
					for todo in &deleted {
						eprintln!("  - {} (msg:{} in group:{}/topic:{})", todo.content, todo.message_id, todo.group_id, todo.topic_id);
					}

					let updates: Vec<_> = deleted
						.iter()
						.map(|todo| sync::MessageUpdate::Delete {
							group_id: todo.group_id,
							topic_id: todo.topic_id,
							message_id: todo.message_id,
						})
						.collect();
					let results = sync::push_via_server(updates, &settings).await?;
					display_push_results(&results);
				}
			}
		},
		Commands::Init(args) => {
			shell_init::output(args);
		}
		Commands::ScheduleUpdate(args) => {
			match args.action {
				UpdateAction::Delete { group_id, topic_id, message_id } => {
					let update = sync::MessageUpdate::Delete { group_id, topic_id, message_id };
					eprintln!("Scheduling delete: msg:{message_id} (group:{group_id}/topic:{topic_id})");
					let results = sync::push_via_server(vec![update], &settings).await?;
					display_push_results(&results);
				}
				UpdateAction::Edit {
					group_id,
					topic_id,
					message_id,
					new_content,
				} => {
					// Look up sender from the topic file
					let metadata = TopicsMetadata::load();
					let chat_filepath = pull::topic_filepath(group_id, topic_id, &metadata);
					let file_content = std::fs::read_to_string(&chat_filepath).unwrap_or_default();
					let messages = sync::parse_file_messages(&file_content);
					let sender = messages.get(&message_id).map(|m| m.sender).unwrap_or(sync::MessageSender::Bot);

					let update = sync::MessageUpdate::Edit {
						group_id,
						topic_id,
						message_id,
						new_content,
						sender,
					};
					eprintln!("Scheduling edit: msg:{message_id} (group:{group_id}/topic:{topic_id})");
					let results = sync::push_via_server(vec![update], &settings).await?;
					display_push_results(&results);
				}
				UpdateAction::Create { group_id, topic_id, content } => {
					// Send through server (which handles file write + telegram send)
					eprintln!("Creating message in group {group_id} topic {topic_id}");

					let message = server::Message::new(group_id, topic_id, content);
					let addr = format!("127.0.0.1:{}", settings.config()?.localhost_port);

					let mut stream = TcpStream::connect(&addr).await.map_err(|_| eyre!("Server not running. Start it with `tg server`"))?;

					let json = serde_json::to_string(&message)?;
					stream.write_all(json.as_bytes()).await?;

					let mut buf = vec![0u8; 4096];
					let n = stream.read(&mut buf).await?;
					if n == 0 {
						bail!("Server closed connection without response");
					}
					let response: server::ServerResponse = serde_json::from_slice(&buf[..n])?;
					sync::check_server_version(&response)?;

					eprintln!("Message queued for sending");
				}
			};
		}
	};

	Ok(())
}

/// Resolve send destination from channel name using metadata
fn resolve_send_destination(args: &SendArgs) -> Result<(u64, u64)> {
	// If direct IDs are provided, use them
	if let Some(group_id) = args.group_id {
		let topic_id = args.topic_id.unwrap_or(1);
		return Ok((group_id, topic_id));
	}

	let pattern = args.channel.as_deref().ok_or_else(|| eyre!("Channel name required (use -c/--channel)"))?;
	let metadata = TopicsMetadata::load();
	let pattern_lower = pattern.to_lowercase();

	// Collect all matching topics from metadata
	let mut matches: Vec<(u64, u64, String)> = Vec::new();
	for (group_id, group) in &metadata.groups {
		for (topic_id, topic_name) in &group.topics {
			if topic_name.to_lowercase().contains(&pattern_lower) {
				let display = format!("{}/{topic_name}", group.name.as_deref().unwrap_or(&format!("group_{}", group_id)));
				matches.push((*group_id, *topic_id, display));
			}
		}
	}

	match matches.len() {
		0 => Err(eyre!("No topic found matching: {pattern}")),
		1 => {
			let (group_id, topic_id, _) = &matches[0];
			Ok((*group_id, *topic_id))
		}
		_ => {
			// Multiple matches - use fzf
			let input: String = matches.iter().map(|(_, _, d)| d.as_str()).collect::<Vec<_>>().join("\n");

			let mut fzf = Command::new("fzf").args(["--query", pattern]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;

			if let Some(stdin) = fzf.stdin.take() {
				let mut stdin_handle = stdin;
				stdin_handle.write_all(input.as_bytes())?;
			}

			let output = fzf.wait_with_output()?;

			if output.status.success() {
				let chosen = String::from_utf8(output.stdout)?.trim().to_string();
				// Find the match
				matches
					.iter()
					.find(|(_, _, d)| d == &chosen)
					.map(|(g, t, _)| (*g, *t))
					.ok_or_else(|| eyre!("Failed to find selection: {chosen}"))
			} else {
				Err(eyre!("No topic selected"))
			}
		}
	}
}

/// Search for topic files using a pattern
fn search_topics_by_pattern(pattern: &str) -> Result<Vec<std::path::PathBuf>> {
	let data_dir = crate::server::DATA_DIR.get().unwrap();

	let output = Command::new("find").args([data_dir.to_str().unwrap(), "-name", "*.md", "-type", "f"]).output()?;

	if !output.status.success() {
		bail!("Failed to search for files");
	}

	let all_files = String::from_utf8(output.stdout)?;
	let mut matches = Vec::new();

	let pattern_lower = pattern.to_lowercase();

	for line in all_files.lines() {
		let file_path = line.trim();
		if file_path.is_empty() {
			continue;
		}

		// Skip images directory
		if file_path.contains("/images/") {
			continue;
		}

		let path = std::path::PathBuf::from(file_path);

		// Get relative path from data_dir for matching
		if let Ok(rel_path) = path.strip_prefix(data_dir) {
			let rel_str = rel_path.to_string_lossy().to_lowercase();

			// Also check just the filename
			let filename = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();

			if rel_str.contains(&pattern_lower) || filename.contains(&pattern_lower) {
				matches.push(path);
			}
		}
	}

	Ok(matches)
}

/// Get all topic files
fn get_all_topic_files() -> Result<Vec<std::path::PathBuf>> {
	let data_dir = crate::server::DATA_DIR.get().unwrap();

	let output = Command::new("find").args([data_dir.to_str().unwrap(), "-name", "*.md", "-type", "f"]).output()?;

	if !output.status.success() {
		bail!("Failed to search for files");
	}

	let all_files = String::from_utf8(output.stdout)?;
	let mut files = Vec::new();

	for line in all_files.lines() {
		let file_path = line.trim();
		if file_path.is_empty() || file_path.contains("/images/") {
			continue;
		}
		files.push(std::path::PathBuf::from(file_path));
	}

	Ok(files)
}

/// Use fzf to let user choose from multiple topic matches
fn choose_topic_with_fzf(matches: &[std::path::PathBuf], initial_query: &str) -> Result<Option<std::path::PathBuf>> {
	let data_dir = crate::server::DATA_DIR.get().unwrap();

	// Prepare input for fzf - use relative paths for display
	let input: String = matches
		.iter()
		.filter_map(|p| p.strip_prefix(data_dir).ok())
		.map(|p| p.to_string_lossy().to_string())
		.collect::<Vec<_>>()
		.join("\n");

	let mut fzf = Command::new("fzf").args(["--query", initial_query]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;

	if let Some(stdin) = fzf.stdin.take() {
		let mut stdin_handle = stdin;
		stdin_handle.write_all(input.as_bytes())?;
	}

	let output = fzf.wait_with_output()?;

	if output.status.success() {
		let chosen = String::from_utf8(output.stdout)?.trim().to_string();
		Ok(Some(data_dir.join(chosen)))
	} else {
		Ok(None)
	}
}

/// Resolve a topic pattern to a file path
fn resolve_topic_path(pattern: Option<&str>) -> Result<std::path::PathBuf> {
	match pattern {
		None => {
			// No pattern: fzf over all files
			let all_files = get_all_topic_files()?;
			if all_files.is_empty() {
				bail!("No topic files found");
			}
			match choose_topic_with_fzf(&all_files, "")? {
				Some(chosen) => Ok(chosen),
				None => bail!("No topic selected"),
			}
		}
		Some(pattern) => {
			let matches = search_topics_by_pattern(pattern)?;

			match matches.len() {
				0 => Err(eyre!("No topics found matching pattern: {pattern}")),
				1 => {
					eprintln!("Found unique match: {}", matches[0].display());
					Ok(matches[0].clone())
				}
				_ => {
					eprintln!("Found {} matches for '{pattern}'. Opening fzf to choose:", matches.len());
					match choose_topic_with_fzf(&matches, pattern)? {
						Some(chosen) => Ok(chosen),
						None => Err(eyre!("No topic selected")),
					}
				}
			}
		}
	}
}

/// List all discovered topics
fn list_topics() -> Result<()> {
	let metadata = TopicsMetadata::load();
	let data_dir = crate::server::DATA_DIR.get().unwrap();

	for (group_id, group) in &metadata.groups {
		let default_name = format!("group_{group_id}");
		let group_name = group.name.as_deref().unwrap_or(&default_name);
		println!("{group_name} ({group_id})");

		for (topic_id, topic_name) in &group.topics {
			let file_path = data_dir.join(group_name).join(format!("{topic_name}.md"));
			let exists = if file_path.exists() { "" } else { " [no file]" };
			println!("  {topic_name} ({topic_id}){exists}");
		}
	}

	Ok(())
}

/// A tracked TODO item (with group/topic/message IDs for syncing)
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TrackedTodo {
	group_id: u64,
	topic_id: u64,
	message_id: i32,
	/// The full message content, used to find and remove from source file
	content: String,
}

/// Parse todos.md and extract all tracked TODO items
fn parse_todos_file(content: &str) -> std::collections::HashSet<TrackedTodo> {
	let mut tracked = std::collections::HashSet::new();
	// Pattern: - [ ] content (date) <!-- todo:group_id:topic_id:msg_id -->
	let todo_re = regex::Regex::new(r"^- \[ \] (.+?) \([A-Za-z]{3} \d{1,2}\) <!-- todo:(\d+):(\d+):(\d+) -->$").unwrap();

	for line in content.lines() {
		if let Some(caps) = todo_re.captures(line.trim()) {
			let content = caps.get(1).unwrap().as_str().to_string();
			if let (Ok(group_id), Ok(topic_id), Ok(msg_id)) = (
				caps.get(2).unwrap().as_str().parse::<u64>(),
				caps.get(3).unwrap().as_str().parse::<u64>(),
				caps.get(4).unwrap().as_str().parse::<i32>(),
			) {
				// Only track items with valid message IDs (not 0)
				if msg_id != 0 {
					tracked.insert(TrackedTodo {
						group_id,
						topic_id,
						message_id: msg_id,
						content,
					});
				}
			}
		}
	}

	tracked
}

/// Build a map from message ID to date by scanning date headers in a topic file.
/// Date headers look like "## Jan 03" or "## Jan 03, 2025".
fn build_message_date_map(content: &str, current_year: i16, today: Date) -> std::collections::HashMap<i32, Date> {
	use std::collections::HashMap;

	let date_header_re = regex::Regex::new(r"^## ([A-Za-z]{3}) (\d{1,2})(?:, (\d{4}))?$").unwrap();
	let msg_id_re = regex::Regex::new(r"<!-- msg:(\d+)").unwrap();

	let mut msg_dates = HashMap::new();
	let mut current_date: Option<Date> = None;

	for line in content.lines() {
		let trimmed = line.trim();

		// Check for date header
		if let Some(caps) = date_header_re.captures(trimmed) {
			let month_str = caps.get(1).unwrap().as_str();
			let day: i8 = caps.get(2).unwrap().as_str().parse().unwrap_or(1);
			let explicit_year: Option<i16> = caps.get(3).and_then(|m| m.as_str().parse().ok());

			let month: i8 = match month_str.to_lowercase().as_str() {
				"jan" => 1,
				"feb" => 2,
				"mar" => 3,
				"apr" => 4,
				"may" => 5,
				"jun" => 6,
				"jul" => 7,
				"aug" => 8,
				"sep" => 9,
				"oct" => 10,
				"nov" => 11,
				"dec" => 12,
				_ => continue,
			};

			let year = explicit_year.unwrap_or_else(|| {
				if let Ok(date) = Date::new(current_year, month, day) {
					if date > today { current_year - 1 } else { current_year }
				} else {
					current_year
				}
			});

			current_date = Date::new(year, month, day).ok();
			continue;
		}

		// Check for message ID and associate with current date
		if let Some(current) = current_date {
			if let Some(caps) = msg_id_re.captures(line) {
				if let Some(id) = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
					msg_dates.insert(id, current);
				}
			}
		}
	}

	msg_dates
}

/// A TODO item extracted from a topic file
struct TodoItem {
	/// The full message content containing the TODO
	content: String,
	/// Source topic path (relative to data dir)
	source: String,
	/// Approximate date of the message containing the TODO
	date: Option<Date>,
	/// Message ID from Telegram (if available)
	message_id: Option<i32>,
	/// Group ID for this TODO's source
	group_id: u64,
	/// Topic ID for this TODO's source
	topic_id: u64,
}

/// Aggregate TODOs from all topic files into todos.md, returning the path
fn aggregate_todos(settings: &LiveSettings) -> Result<std::path::PathBuf> {
	use std::io::Read as _;

	let cfg = settings.config()?;
	let data_dir = crate::server::DATA_DIR.get().unwrap();
	let cutoff_duration = cfg.pull_todos_over().duration();
	let today = Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date();
	let cutoff_span = jiff::Span::try_from(cutoff_duration).unwrap_or_else(|_| 1.week());
	let cutoff_date = today.checked_sub(cutoff_span).unwrap_or(today);

	let metadata = TopicsMetadata::load();
	let mut todos: Vec<TodoItem> = Vec::new();

	let current_year = today.year();

	// Iterate over all known topics from metadata
	for (group_id, group) in &metadata.groups {
		for (topic_id, topic_name) in &group.topics {
			let file_path = pull::topic_filepath(*group_id, *topic_id, &metadata);
			let source = format!("{}/{topic_name}.md", group.name.as_deref().unwrap_or(&format!("group_{}", group_id)));

			let mut contents = String::new();
			if std::fs::File::open(&file_path).and_then(|mut f| f.read_to_string(&mut contents)).is_err() {
				continue;
			}

			// Build a map of message_id -> date by scanning date headers
			let msg_dates = build_message_date_map(&contents, current_year, today);

			// Use parse_file_messages to get clean message content (tags already stripped)
			let messages = sync::parse_file_messages(&contents);

			// Find all messages containing "TODO:"
			for (msg_id, parsed) in &messages {
				if parsed.content.contains("TODO:") {
					let date = msg_dates.get(msg_id).copied();
					let include = date.map(|d| d >= cutoff_date).unwrap_or(true);
					if include {
						todos.push(TodoItem {
							content: parsed.content.clone(),
							source: source.clone(),
							date,
							message_id: Some(*msg_id),
							group_id: *group_id,
							topic_id: *topic_id,
						});
					}
				}
			}
		}
	}

	// Sort by date (newest first), then by source
	todos.sort_by(|a, b| match (&b.date, &a.date) {
		(Some(bd), Some(ad)) => bd.cmp(ad).then_with(|| a.source.cmp(&b.source)),
		(Some(_), None) => std::cmp::Ordering::Less,
		(None, Some(_)) => std::cmp::Ordering::Greater,
		(None, None) => a.source.cmp(&b.source),
	});

	// Write todos.md
	let todos_path = data_dir.join("todos.md");
	let mut output = String::new();
	output.push_str("# TODOs\n");
	output.push_str(&format!("*Auto-aggregated from group messages (last {}).*\n\n", cfg.pull_todos_over()));

	if todos.is_empty() {
		output.push_str("No TODOs found.\n");
	} else {
		let mut current_source: Option<&str> = None;
		for todo in &todos {
			if current_source != Some(&todo.source) {
				if current_source.is_some() {
					output.push('\n');
				}
				output.push_str(&format!("## {}\n", todo.source));
				current_source = Some(&todo.source);
			}

			let date_str = todo.date.map(|d| format!(" ({})", d.strftime("%b %d"))).unwrap_or_default();
			// Include tracking info: group_id:topic_id:msg_id (msg_id is 0 if not available)
			let msg_id = todo.message_id.unwrap_or(0);
			let tracking = format!(" <!-- todo:{}:{}:{msg_id} -->", todo.group_id, todo.topic_id);
			output.push_str(&format!("- [ ] {}{date_str}{tracking}\n", todo.content));
		}
	}

	std::fs::write(&todos_path, &output)?;
	if todos.is_empty() {
		println!("No TODOs found");
	} else {
		println!("Wrote {} TODOs to {}", todos.len(), todos_path.display());
	}

	Ok(todos_path)
}

/// Display push operation results to the user
fn display_push_results(results: &PushResults) {
	if results.deletions.is_empty() && results.edits.is_empty() && results.creates.is_empty() {
		return;
	}

	eprintln!("\nResults:");

	for (group_id, msg_id, op) in &results.deletions {
		let status = if op.success { "✓" } else { "✗" };
		eprintln!("  {status} delete msg:{msg_id} (group:{group_id}): {}", op.message);
	}

	for (group_id, msg_id, op) in &results.edits {
		let status = if op.success { "✓" } else { "✗" };
		eprintln!("  {status} edit msg:{msg_id} (group:{group_id}): {}", op.message);
	}

	for (group_id, topic_id, op) in &results.creates {
		let status = if op.success { "✓" } else { "✗" };
		eprintln!("  {status} create (group:{group_id}/topic:{topic_id}): {}", op.message);
	}

	// File cleanups
	for (path, lines_removed, message) in &results.file_cleanups {
		let status = if *lines_removed > 0 { "✓" } else { "✗" };
		eprintln!("  {status} cleanup {path}: {message}");
	}

	// Summary
	let del_ok = results.deletions.iter().filter(|(_, _, r)| r.success).count();
	let del_fail = results.deletions.len() - del_ok;
	let edit_ok = results.edits.iter().filter(|(_, _, r)| r.success).count();
	let edit_fail = results.edits.len() - edit_ok;
	let create_ok = results.creates.iter().filter(|(_, _, r)| r.success).count();
	let create_fail = results.creates.len() - create_ok;

	let mut summary_parts = Vec::new();
	if !results.deletions.is_empty() {
		if del_fail > 0 {
			summary_parts.push(format!("{del_ok}/{} deletions", results.deletions.len()));
		} else {
			summary_parts.push(format!("{del_ok} deletions"));
		}
	}
	if !results.edits.is_empty() {
		if edit_fail > 0 {
			summary_parts.push(format!("{edit_ok}/{} edits", results.edits.len()));
		} else {
			summary_parts.push(format!("{edit_ok} edits"));
		}
	}
	if !results.creates.is_empty() {
		if create_fail > 0 {
			summary_parts.push(format!("{create_ok}/{} creates", results.creates.len()));
		} else {
			summary_parts.push(format!("{create_ok} creates"));
		}
	}

	if !summary_parts.is_empty() {
		eprintln!("Summary: {}", summary_parts.join(", "));
	}
}
