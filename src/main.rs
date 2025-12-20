use std::{
	io::Write as IoWrite,
	path::Path,
	process::{Command, Stdio},
};

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
			let (group_id, topic_id) = resolve_send_destination(&args)?;

			let message = Message::new(group_id, topic_id, args.message.join(" "));
			let addr = format!("127.0.0.1:{}", config.localhost_port);
			let mut stream = TcpStream::connect(addr).await?;

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
	};

	Ok(())
}

/// Resolve send destination from args
fn resolve_send_destination(args: &SendArgs) -> Result<(u64, u64)> {
	// If direct IDs are provided, use them
	if let Some(group_id) = args.group_id {
		let topic_id = args.topic_id.unwrap_or(1); // Default to General topic
		return Ok((group_id, topic_id));
	}

	// Otherwise, resolve from pattern
	let path = resolve_topic_path(args.topic.as_deref())?;

	// Parse group_id and topic_id from the file path
	parse_ids_from_path(&path)
}

/// Parse group_id and topic_id from a topic file path
fn parse_ids_from_path(path: &Path) -> Result<(u64, u64)> {
	let metadata = TopicsMetadata::load();
	let data_dir = crate::server::DATA_DIR.get().unwrap();

	// Get relative path from data_dir
	let rel_path = path.strip_prefix(data_dir).map_err(|_| eyre!("Path not in data directory"))?;

	// Expected format: {group_name}/{topic_name}.md
	let components: Vec<_> = rel_path.components().collect();
	if components.len() != 2 {
		bail!("Invalid path format, expected group/topic.md");
	}

	let group_name = components[0].as_os_str().to_string_lossy();
	let topic_filename = components[1].as_os_str().to_string_lossy();
	let topic_name = topic_filename.strip_suffix(".md").unwrap_or(&topic_filename);

	// Find group_id from group_name
	let group_id = metadata
		.groups
		.iter()
		.find(|(_, g)| g.name.as_deref() == Some(&*group_name) || format!("group_{}", **&metadata.groups.keys().next().unwrap()) == *group_name)
		.map(|(id, _)| *id)
		.or_else(|| {
			// Try parsing as group_{id}
			group_name.strip_prefix("group_").and_then(|s| s.parse().ok())
		})
		.ok_or_else(|| eyre!("Could not find group_id for '{}'", group_name))?;

	// Find topic_id from topic_name
	let topic_id = metadata
		.groups
		.get(&group_id)
		.and_then(|g| g.topics.iter().find(|(_, name)| *name == topic_name).map(|(id, _)| *id))
		.or_else(|| {
			// Try parsing as topic_{id}
			topic_name.strip_prefix("topic_").and_then(|s| s.parse().ok())
		})
		.ok_or_else(|| eyre!("Could not find topic_id for '{}' in group {}", topic_name, group_id))?;

	Ok((group_id, topic_id))
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
