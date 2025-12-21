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
	io::{ExpandedPath, OpenMode, open_with_mode},
	trades::Timeframe,
};

use crate::config::TopicsMetadata;

pub mod backfill;
pub mod config;
mod server;

#[derive(Clone, Debug, Parser)]
#[command(author, version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")"), about, long_about = None)]
struct Cli {
	#[command(subcommand)]
	command: Commands,
	#[arg(long, default_value = "~/.config/tg.toml")]
	config: ExpandedPath,
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
	/// Backfill messages from Telegram for all configured forum groups
	Backfill,
	/// List all discovered topics
	List,
	/// Aggregate TODOs from all topics into todos.md
	Todos,
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
struct ServerArgs {
	/// Interval for periodic backfill from Telegram (e.g., "1m", "5m", "1h")
	#[arg(long, default_value = "1m")]
	backfill_interval: Timeframe,
}

#[tokio::main]
async fn main() -> Result<()> {
	v_utils::clientside!();
	let cli = Cli::parse();
	let config = config::AppConfig::read(&cli.config.0).expect("Failed to read config file");
	let bot_token = match cli.token {
		Some(t) => t,
		None => std::env::var("TELEGRAM_BOT_KEY").expect("TELEGRAM_BOT_KEY not set"),
	};

	match cli.command {
		Commands::Send(args) => {
			let (group_id, topic_id) = resolve_send_destination(&args, &config)?;
			let msg_text = args.message.join(" ");

			let message = Message::new(group_id, topic_id, msg_text.clone());
			let addr = format!("127.0.0.1:{}", config.localhost_port());
			let mut stream = TcpStream::connect(&addr).await?;

			let json = serde_json::to_string(&message)?;
			stream.write_all(json.as_bytes()).await?;

			let mut response = [0u8; 3];
			stream.read_exact(&mut response).await?;
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
			server::run(config, bot_token, args.backfill_interval).await?;
		}
		Commands::Backfill => {
			backfill::backfill(&config, &bot_token).await?;
		}
		Commands::Open(args) => {
			let path = resolve_topic_path(args.pattern.as_deref())?;
			open_with_mode(&path, OpenMode::Normal)?;
		}
		Commands::List => {
			list_topics()?;
		}
		Commands::Todos => {
			aggregate_todos(&config)?;
		}
	};

	Ok(())
}

/// Resolve send destination from args using config channels
fn resolve_send_destination(args: &SendArgs, config: &config::AppConfig) -> Result<(u64, u64)> {
	// If direct IDs are provided, use them
	if let Some(group_id) = args.group_id {
		let topic_id = args.topic_id.unwrap_or(1);
		return Ok((group_id, topic_id));
	}

	// Resolve from channel name in config
	let name = args.topic.as_deref().ok_or_else(|| eyre!("Channel name required"))?;

	// Try exact match first
	if let Some((group_id, topic_id)) = config.resolve_channel(name) {
		return Ok((group_id, topic_id));
	}

	// Try fuzzy match
	let channels = config.channels.as_ref().ok_or_else(|| eyre!("No channels configured"))?;
	let name_lower = name.to_lowercase();
	let matches: Vec<_> = channels.iter().filter(|(k, _)| k.to_lowercase().contains(&name_lower)).collect();

	match matches.len() {
		0 => Err(eyre!("No channel found matching: {}", name)),
		1 => {
			let (channel_name, dest) = matches[0];
			eprintln!("Found: {}", channel_name);
			config::parse_channel_destination(dest).ok_or_else(|| eyre!("Invalid channel destination: {}", dest))
		}
		_ => {
			// Multiple matches - use fzf
			let input: String = matches.iter().map(|(k, v)| format!("{} -> {}", k, v)).collect::<Vec<_>>().join("\n");

			let mut fzf = Command::new("fzf").args(["--query", name]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;

			if let Some(stdin) = fzf.stdin.take() {
				let mut stdin_handle = stdin;
				stdin_handle.write_all(input.as_bytes())?;
			}

			let output = fzf.wait_with_output()?;

			if output.status.success() {
				let chosen = String::from_utf8(output.stdout)?.trim().to_string();
				let channel_name = chosen.split(" -> ").next().unwrap_or("");
				config.resolve_channel(channel_name).ok_or_else(|| eyre!("Failed to resolve: {}", channel_name))
			} else {
				Err(eyre!("No channel selected"))
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

/// A TODO item extracted from a topic file
struct TodoItem {
	/// The TODO text (after "TODO: ")
	text: String,
	/// Source topic path (relative to data dir)
	source: String,
	/// Approximate date of the message containing the TODO
	date: Option<chrono::NaiveDate>,
}

/// Aggregate TODOs from all topic files into todos.md
fn aggregate_todos(config: &config::AppConfig) -> Result<()> {
	use std::io::Read as _;

	let data_dir = crate::server::DATA_DIR.get().unwrap();
	let cutoff_duration = config.pull_todos_over().duration();
	let cutoff_date = chrono::Utc::now().naive_utc().date() - chrono::Duration::from_std(cutoff_duration).unwrap_or(chrono::Duration::weeks(1));

	let metadata = TopicsMetadata::load();
	let mut todos: Vec<TodoItem> = Vec::new();

	// Regex for date headers like "## Jan 03" or "## Dec 25"
	let date_header_re = regex::Regex::new(r"^## ([A-Za-z]{3}) (\d{1,2})$").unwrap();

	// Iterate over all known topics from metadata
	for (group_id, group) in &metadata.groups {
		for (topic_id, topic_name) in &group.topics {
			let file_path = backfill::topic_filepath(*group_id, *topic_id, &metadata);
			let source = format!("{}/{}.md", group.name.as_deref().unwrap_or(&format!("group_{}", group_id)), topic_name);

			let mut contents = String::new();
			if std::fs::File::open(&file_path).and_then(|mut f| f.read_to_string(&mut contents)).is_err() {
				continue;
			}

			let mut current_date: Option<chrono::NaiveDate> = None;
			let current_year = chrono::Utc::now().year();

			for line in contents.lines() {
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
					let todo_text = trimmed[todo_start + 6..].trim();
					if !todo_text.is_empty() {
						// Only include if within cutoff date (or no date info available)
						let include = current_date.map(|d| d >= cutoff_date).unwrap_or(true);
						if include {
							todos.push(TodoItem {
								text: todo_text.to_string(),
								source: source.clone(),
								date: current_date,
							});
						}
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
	output.push_str("# TODOs\n\n");
	output.push_str(&format!("*Auto-aggregated from channel messages (last {}).*\n\n", config.pull_todos_over()));

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
			output.push_str(&format!("- [ ] {}{}\n", todo.text, date_str));
		}
	}

	std::fs::write(&todos_path, &output)?;
	println!("Wrote {} TODOs to {}", todos.len(), todos_path.display());

	Ok(())
}
