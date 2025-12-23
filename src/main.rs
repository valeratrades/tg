use std::{
	io::Write as IoWrite,
	process::{Command, Stdio},
};

use chrono::Datelike;
use clap::{Args, Parser, Subcommand};
use eyre::{Result, bail, eyre};
use server::Message;
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::TcpStream,
};
use v_utils::{
	io::{OpenMode, open_with_mode},
	trades::Timeframe,
};

use crate::config::{AppConfig, SettingsFlags, TopicsMetadata};

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
	/// Send a message to a topic
	/// Ex:
	/// ```sh
	/// tg send journal "today I'm feeling blue"
	/// tg send -g 2244305221 -t 7 "direct message"
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
}

#[derive(Args, Clone, Debug)]
struct SendArgs {
	/// Pattern to match topic name (uses fzf if multiple matches)
	topic: Option<String>,
	/// Direct group ID (bypasses pattern matching)
	#[arg(short, long)]
	group_id: Option<u64>,
	/// Direct topic ID (requires --group-id)
	#[arg(short, long)]
	topic_id: Option<u64>,
	/// Message to send
	#[arg(allow_hyphen_values = true)]
	message: Vec<String>,
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

#[tokio::main]
async fn main() -> Result<()> {
	v_utils::clientside!();
	let cli = Cli::parse();
	let config = AppConfig::try_build(cli.settings).expect("Failed to read config file");
	config::init_data_dir();
	let bot_token = match cli.token {
		Some(t) => t,
		None => std::env::var("TELEGRAM_MAIN_BOT_TOKEN").expect("TELEGRAM_MAIN_BOT_TOKEN not set"),
	};

	match cli.command {
		Commands::Send(args) => {
			let (group_id, topic_id) = resolve_send_destination(&args)?;
			let msg_text = args.message.join(" ");

			let message = Message::new(group_id, topic_id, msg_text.clone());
			let addr = format!("127.0.0.1:{}", config.localhost_port);

			// Try server first, fall back to direct send
			match TcpStream::connect(&addr).await {
				Ok(mut stream) => {
					let json = serde_json::to_string(&message)?;
					stream.write_all(json.as_bytes()).await?;
					let mut response = [0u8; 3];
					stream.read_exact(&mut response).await?;
				}
				Err(_) => {
					// Server not running, send directly
					server::send_message(&message, &bot_token).await?;
				}
			}
		}
		Commands::BotInfo => {
			let url = format!("https://api.telegram.org/bot{bot_token}/getMe");
			let client = reqwest::Client::new();
			let res = client.get(&url).send().await?;

			let parsed_json: serde_json::Value = serde_json::from_str(&res.text().await?).expect("Failed to parse JSON");
			let pretty_json = serde_json::to_string_pretty(&parsed_json).expect("Failed to pretty print JSON");
			println!("{pretty_json}");
		}
		Commands::Server(args) => {
			server::run(config, bot_token, args.pull_interval).await?;
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

				// Clear sync timestamps
				let sync_path = pull::SyncTimestamps::file_path();
				if sync_path.exists() {
					std::fs::remove_file(&sync_path)?;
					eprintln!("Cleared sync timestamps");
				}

				// Clear topic files
				let data_dir = server::DATA_DIR.get().unwrap();
				let metadata = TopicsMetadata::load();
				for (group_id, group) in &metadata.groups {
					let default_name = format!("group_{}", group_id);
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
			pull::pull(&config, &bot_token).await?;
		}
		Commands::Open(args) => {
			let path = resolve_topic_path(args.pattern.as_deref())?;

			// Read and parse file content before opening
			let old_content = std::fs::read_to_string(&path).unwrap_or_default();
			let old_state = sync::parse_file_messages(&old_content);

			// Open with editor
			open_with_mode(&path, OpenMode::Normal)?;

			// Read and parse file content after closing editor
			let new_content = std::fs::read_to_string(&path).unwrap_or_default();
			let new_state = sync::parse_file_messages(&new_content);

			// Detect changes
			let changes = sync::detect_changes(&old_state, &new_state);

			if !changes.is_empty() {
				// Resolve group ID from file path
				if let Some((group_id, _topic_id)) = sync::resolve_topic_ids_from_path(&path) {
					eprintln!(
						"Detected {} changes ({} deletions, {} edits)",
						changes.total_affected(),
						changes.deleted.len(),
						changes.edited.len()
					);

					// Apply changes via MTProto
					let (client, handle) = mtproto::create_client(&config).await?;
					sync::apply_changes(&changes, group_id, &client).await?;
					client.disconnect();
					handle.abort();
				} else {
					eprintln!("Warning: Could not resolve topic IDs from path, changes not synced to Telegram");
				}
			}
		}
		Commands::List => {
			list_topics()?;
		}
		Commands::Todos(cmd) => match cmd {
			TodosCommands::Compile => {
				aggregate_todos(&config)?;
			}
			TodosCommands::Open => {
				let path = aggregate_todos(&config)?;

				// Read the generated todos.md to get the old state
				let old_content = std::fs::read_to_string(&path).unwrap_or_default();
				let old_todos = parse_todos_file(&old_content);

				// Count total TODOs vs trackable ones
				let total_todo_lines = old_content.lines().filter(|l| l.contains("<!-- todo:")).count();
				let trackable_count = old_todos.len();
				let untrackable_count = total_todo_lines - trackable_count;
				tracing::debug!(
					total_todos = total_todo_lines,
					trackable = trackable_count,
					untrackable = untrackable_count,
					"Parsed todos.md before edit"
				);

				if trackable_count == 0 && total_todo_lines > 0 {
					eprintln!("Note: {} TODOs found but none have message IDs (old messages before ID tracking).", total_todo_lines);
					eprintln!("Deletions from todos.md won't sync to Telegram for these items.");
				} else if untrackable_count > 0 {
					eprintln!(
						"Note: {}/{} TODOs have message IDs. {} cannot be synced (old messages).",
						trackable_count, total_todo_lines, untrackable_count
					);
				}

				// Open with editor
				open_with_mode(&path, OpenMode::Normal)?;

				// Read the file after editing
				let new_content = std::fs::read_to_string(&path).unwrap_or_default();
				let new_todos = parse_todos_file(&new_content);

				// Find deleted TODOs (in old but not in new)
				let deleted: Vec<_> = old_todos.difference(&new_todos).cloned().collect();

				tracing::debug!(
					old_trackable = old_todos.len(),
					new_trackable = new_todos.len(),
					deleted_count = deleted.len(),
					"Comparing todos after edit"
				);
				for todo in &deleted {
					tracing::debug!(group_id = todo.group_id, topic_id = todo.topic_id, message_id = todo.message_id, "Will delete todo");
				}

				eprintln!("Tracking: {} trackable TODOs before, {} after, {} deleted", old_todos.len(), new_todos.len(), deleted.len());

				if !deleted.is_empty() {
					eprintln!("Syncing {} deletion(s) to Telegram via MTProto...", deleted.len());

					// Group deletions by group_id (MTProto deletes by channel, not topic)
					let mut deletions_by_group: std::collections::BTreeMap<u64, Vec<i32>> = std::collections::BTreeMap::new();
					for todo in &deleted {
						deletions_by_group.entry(todo.group_id).or_default().push(todo.message_id);
					}

					// Create MTProto client and apply deletions
					let (client, handle) = mtproto::create_client(&config).await?;

					for (group_id, msg_ids) in &deletions_by_group {
						tracing::info!(group_id, msg_ids = ?msg_ids, "Deleting messages via MTProto");
						match mtproto::delete_messages(&client, *group_id, msg_ids).await {
							Ok(count) => {
								tracing::info!(group_id, count, "Successfully deleted messages");
								eprintln!("Deleted {} message(s) from group {}", count, group_id);
							}
							Err(e) => {
								tracing::error!(group_id, error = %e, "Failed to delete messages");
								eprintln!("Failed to delete from group {}: {}", group_id, e);
							}
						}
					}

					client.disconnect();
					handle.abort();

					// Also remove TODOs from source topic files so they don't reappear
					let mut source_removals = 0;
					for todo in &deleted {
						match remove_todo_from_source(todo.group_id, todo.topic_id, &todo.text) {
							Ok(true) => source_removals += 1,
							Ok(false) => tracing::warn!(text = %todo.text, "TODO not found in source file"),
							Err(e) => tracing::error!(error = %e, "Failed to remove TODO from source file"),
						}
					}
					if source_removals > 0 {
						eprintln!("Removed {} TODO(s) from source topic files", source_removals);
					}
				}
			}
		},
		Commands::Init(args) => {
			shell_init::output(args);
		}
	};

	Ok(())
}

/// Resolve send destination from topic name using metadata
fn resolve_send_destination(args: &SendArgs) -> Result<(u64, u64)> {
	// If direct IDs are provided, use them
	if let Some(group_id) = args.group_id {
		let topic_id = args.topic_id.unwrap_or(1);
		return Ok((group_id, topic_id));
	}

	let pattern = args.topic.as_deref().ok_or_else(|| eyre!("Topic name required"))?;
	let metadata = TopicsMetadata::load();
	let pattern_lower = pattern.to_lowercase();

	// Collect all matching topics from metadata
	let mut matches: Vec<(u64, u64, String)> = Vec::new();
	for (group_id, group) in &metadata.groups {
		for (topic_id, topic_name) in &group.topics {
			if topic_name.to_lowercase().contains(&pattern_lower) {
				let display = format!("{}/{}", group.name.as_deref().unwrap_or(&format!("group_{}", group_id)), topic_name);
				matches.push((*group_id, *topic_id, display));
			}
		}
	}

	match matches.len() {
		0 => Err(eyre!("No topic found matching: {}", pattern)),
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
					.ok_or_else(|| eyre!("Failed to find selection: {}", chosen))
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
		return Err(eyre!("Failed to search for files"));
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
		return Err(eyre!("Failed to search for files"));
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
				0 => Err(eyre!("No topics found matching pattern: {}", pattern)),
				1 => {
					eprintln!("Found unique match: {}", matches[0].display());
					Ok(matches[0].clone())
				}
				_ => {
					eprintln!("Found {} matches for '{}'. Opening fzf to choose:", matches.len(), pattern);
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
		let default_name = format!("group_{}", group_id);
		let group_name = group.name.as_deref().unwrap_or(&default_name);
		println!("{} ({})", group_name, group_id);

		for (topic_id, topic_name) in &group.topics {
			let file_path = data_dir.join(group_name).join(format!("{}.md", topic_name));
			let exists = if file_path.exists() { "" } else { " [no file]" };
			println!("  {} ({}){}", topic_name, topic_id, exists);
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
	/// The TODO text, used to find and remove from source file
	text: String,
}

/// Parse todos.md and extract all tracked TODO items
fn parse_todos_file(content: &str) -> std::collections::HashSet<TrackedTodo> {
	let mut tracked = std::collections::HashSet::new();
	// Pattern: - [ ] text (date) <!-- todo:group_id:topic_id:msg_id -->
	let todo_re = regex::Regex::new(r"^- \[ \] (.+?) \([A-Za-z]{3} \d{1,2}\) <!-- todo:(\d+):(\d+):(\d+) -->$").unwrap();

	for line in content.lines() {
		if let Some(caps) = todo_re.captures(line.trim()) {
			let text = caps.get(1).unwrap().as_str().to_string();
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
						text,
					});
				}
			}
		}
	}

	tracked
}

/// Remove a TODO line from a source topic file
fn remove_todo_from_source(group_id: u64, topic_id: u64, todo_text: &str) -> Result<bool> {
	let metadata = TopicsMetadata::load();
	let file_path = pull::topic_filepath(group_id, topic_id, &metadata);

	if !file_path.exists() {
		return Ok(false);
	}

	let content = std::fs::read_to_string(&file_path)?;
	let mut lines: Vec<&str> = content.lines().collect();
	let original_len = lines.len();

	// Find and remove the line containing this TODO text
	// Match "TODO: {text}" pattern
	let search_pattern = format!("TODO: {}", todo_text);
	lines.retain(|line| !line.contains(&search_pattern));

	if lines.len() < original_len {
		// Something was removed, write back
		let new_content = lines.join("\n");
		std::fs::write(&file_path, new_content)?;
		tracing::info!(path = %file_path.display(), "Removed TODO from source file");
		return Ok(true);
	}

	Ok(false)
}

/// A TODO item extracted from a topic file
struct TodoItem {
	/// The TODO text (after "TODO: ")
	text: String,
	/// Source topic path (relative to data dir)
	source: String,
	/// Approximate date of the message containing the TODO
	date: Option<chrono::NaiveDate>,
	/// Message ID from Telegram (if available)
	message_id: Option<i32>,
	/// Group ID for this TODO's source
	group_id: u64,
	/// Topic ID for this TODO's source
	topic_id: u64,
}

/// Aggregate TODOs from all topic files into todos.md, returning the path
fn aggregate_todos(config: &config::AppConfig) -> Result<std::path::PathBuf> {
	use std::io::Read as _;

	let data_dir = crate::server::DATA_DIR.get().unwrap();
	let cutoff_duration = config.pull_todos_over().duration();
	let cutoff_date = chrono::Utc::now().naive_utc().date() - chrono::Duration::from_std(cutoff_duration).unwrap_or(chrono::Duration::weeks(1));

	let metadata = TopicsMetadata::load();
	let mut todos: Vec<TodoItem> = Vec::new();

	// Regex for date headers like "## Jan 03" or "## Dec 25"
	let date_header_re = regex::Regex::new(r"^## ([A-Za-z]{3}) (\d{1,2})$").unwrap();
	// Regex for message ID markers like <!-- msg:123 -->
	let msg_id_re = regex::Regex::new(r"<!-- msg:(\d+) -->").unwrap();
	// Regex for message block start (`. ` prefix or date header)
	let msg_block_start_re = regex::Regex::new(r"^(\. |## )").unwrap();

	// Iterate over all known topics from metadata
	for (group_id, group) in &metadata.groups {
		for (topic_id, topic_name) in &group.topics {
			let file_path = pull::topic_filepath(*group_id, *topic_id, &metadata);
			let source = format!("{}/{}.md", group.name.as_deref().unwrap_or(&format!("group_{}", group_id)), topic_name);

			let mut contents = String::new();
			if std::fs::File::open(&file_path).and_then(|mut f| f.read_to_string(&mut contents)).is_err() {
				continue;
			}

			let mut current_date: Option<chrono::NaiveDate> = None;
			let current_year = chrono::Utc::now().year();

			// Parse file into message blocks to associate TODOs with their message IDs
			// A message block is: all lines from one message start until the next
			// Message ID marker appears at the end of the message block
			let lines: Vec<&str> = contents.lines().collect();
			let mut block_start = 0;

			for i in 0..=lines.len() {
				// Check if this is a block boundary (new block starts or end of file)
				let is_boundary = i == lines.len() || (i > 0 && msg_block_start_re.is_match(lines[i]));

				if is_boundary && block_start < i {
					// Process the block from block_start to i-1
					let block_lines = &lines[block_start..i];

					// Find message ID in the block (usually on the last line)
					let mut block_msg_id: Option<i32> = None;
					for line in block_lines.iter().rev() {
						if let Some(caps) = msg_id_re.captures(line) {
							block_msg_id = caps.get(1).and_then(|m| m.as_str().parse().ok());
							break;
						}
					}

					// Process lines in this block
					for line in block_lines {
						let trimmed = line.trim();

						// Check for date header
						if let Some(caps) = date_header_re.captures(trimmed) {
							let month_str = caps.get(1).unwrap().as_str();
							let day: u32 = caps.get(2).unwrap().as_str().parse().unwrap_or(1);

							let month = match month_str.to_lowercase().as_str() {
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

							// Assume current year, but handle year boundary
							let mut year = current_year;
							if let Some(date) = chrono::NaiveDate::from_ymd_opt(year, month, day) {
								// If this date is in the future, it's probably from last year
								if date > chrono::Utc::now().naive_utc().date() {
									year -= 1;
								}
								current_date = chrono::NaiveDate::from_ymd_opt(year, month, day);
							}
							continue;
						}

						// Check for TODO
						if let Some(todo_start) = trimmed.find("TODO: ") {
							// Extract TODO text (remove the msg ID marker if present)
							let todo_line = msg_id_re.replace(trimmed, "");
							let todo_text = todo_line[todo_start + 6..].trim();

							if !todo_text.is_empty() {
								// Only include if within cutoff date (or no date info available)
								let include = current_date.map(|d| d >= cutoff_date).unwrap_or(true);
								if include {
									todos.push(TodoItem {
										text: todo_text.to_string(),
										source: source.clone(),
										date: current_date,
										message_id: block_msg_id,
										group_id: *group_id,
										topic_id: *topic_id,
									});
								}
							}
						}
					}

					block_start = i;
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
	output.push_str("# TODOs\n\n");
	output.push_str(&format!("*Auto-aggregated from group messages (last {}).*\n\n", config.pull_todos_over()));

	if todos.is_empty() {
		output.push_str("No TODOs found.\n");
	} else {
		let mut current_source: Option<&str> = None;
		for todo in &todos {
			if current_source != Some(&todo.source) {
				if current_source.is_some() {
					output.push('\n');
				}
				output.push_str(&format!("## {}\n\n", todo.source));
				current_source = Some(&todo.source);
			}

			let date_str = todo.date.map(|d| format!(" ({})", d.format("%b %d"))).unwrap_or_default();
			// Include tracking info: group_id:topic_id:msg_id (msg_id is 0 if not available)
			let msg_id = todo.message_id.unwrap_or(0);
			let tracking = format!(" <!-- todo:{}:{}:{} -->", todo.group_id, todo.topic_id, msg_id);
			output.push_str(&format!("- [ ] {}{}{}\n", todo.text, date_str, tracking));
		}
	}

	std::fs::write(&todos_path, &output)?;
	println!("Wrote {} TODOs to {}", todos.len(), todos_path.display());

	Ok(todos_path)
}
