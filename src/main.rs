#[allow(unused_imports)] // std::str::FromStr is not detected by RA correctly (2025/03/21)
use std::str::FromStr;

use clap::{Args, Parser, Subcommand};
use eyre::{Result, bail, eyre};
use server::Message;
use tg::chat::TelegramDestination;
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::TcpStream,
};
use v_utils::{
	io::{ExpandedPath, OpenMode, open_with_mode},
	trades::Timeframe,
};
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
	/// Send the message (if more than one string is provided, they are contatenated with a space)
	///Ex
	///```sh
	///tg send -c journal "today I'm feeling blue" "//this is still a part of the message"
	///```
	Send(SendArgs),
	/// Get information about the bot
	BotInfo,
	/// List all the channels defined in the config file
	ListChannels,
	/// Gen aliases for sending to channels
	GenAliases,
	/// Start a telegram server, compiling messages from the configured channels into markdown log files, to be viewed with $EDITOR
	Server(ServerArgs),
	/// Open the messages file of a channel
	Open(OpenArgs),
	/// Backfill messages from Telegram for all configured channels
	Backfill,
}
#[derive(Args, Clone, Debug)]
struct SendArgs {
	/// Name of the channel to send the message to. Matches against the keys in the config file
	#[arg(short, long)]
	channel: Option<String>,
	/// Telegram Destination, following same formats as the config file
	#[arg(short, long)]
	destination: Option<TelegramDestination>,
	/// Message to send
	#[arg(allow_hyphen_values = true)]
	message: Vec<String>,
}

#[derive(Args, Clone, Debug)]
struct OpenArgs {
	/// Name of the channel to open
	channel: Option<String>,
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
	//TODO!: make it possible to define the bot_token inside the config too (env overwrites if exists)
	let bot_token = match cli.token {
		Some(t) => t,
		None => std::env::var("TELEGRAM_BOT_KEY").expect("TELEGRAM_BOT_KEY not set"),
	};

	match cli.command {
		Commands::Send(args) => {
			let try_extract_destination_from_args: Result<TelegramDestination> = {
				match (&args.channel, &args.destination) {
					(Some(channel), None) => match config.channels.get(channel) {
						Some(d) => Ok(d.clone()),
						None => Err(eyre!("Channel not found in config")),
					},
					(None, Some(destination)) => Ok(destination.clone()),
					_ => Err(eyre!("One and only one of --channel and --destination must be provided")),
				}
			};

			let destination: TelegramDestination = match try_extract_destination_from_args {
				Ok(d) => d,
				Err(e) => {
					eprintln!("{e}");
					std::process::exit(1);
				}
			};

			let message = Message::new(destination, args.message.join(" "));
			let addr = format!("127.0.0.1:{}", config.localhost_port);
			let mut stream = TcpStream::connect(addr).await?;

			let json = serde_json::to_string(&message)?;
			stream.write_all(json.as_bytes()).await?;

			let mut response = [0u8; 3]; // Buffer to read "200"
			stream.read_exact(&mut response).await?;

			let _response_str = String::from_utf8_lossy(&response);
		}
		Commands::BotInfo => {
			let url = format!("https://api.telegram.org/bot{bot_token}/getMe");
			let client = reqwest::Client::new();
			let res = client.get(&url).send().await?;

			let parsed_json: serde_json::Value = serde_json::from_str(&res.text().await?).expect("Failed to parse JSON");
			let pretty_json = serde_json::to_string_pretty(&parsed_json).expect("Failed to pretty print JSON");
			println!("{pretty_json}");
		}
		Commands::ListChannels => {
			let mut s = String::new();
			config.channels.iter().for_each(|(k, _)| {
				if !s.is_empty() {
					s.push_str(", ");
				}
				s.push_str(k);
			});
			println!("{s}");
		}
		Commands::GenAliases => {
			let mut s = String::new();
			for (key, _) in config.channels.iter() {
				// alias to "t{first letter of the key with which the alias is not taken}"
				let mut key_chars = key.chars();
				loop {
					match key_chars.next() {
						Some(c) => {
							let try_alias = format!("tg{c}");

							let alias_already_exists = std::process::Command::new("env")
								.arg("sh")
								.arg("-c")
								.arg(format!("which {try_alias} 2>/dev/null"))
								.output()
								.expect("Failed to execute command")
								.status
								.success();

							let alias_in_suggested = s.contains(&format!("alias {try_alias}=")) || try_alias == "tgo";
							if alias_already_exists || alias_in_suggested {
								continue;
							} else {
								if !s.is_empty() {
									s.push('\n');
								}
								s.push_str(&format!("alias {try_alias}=\"tg send -c {key} >/dev/null\""));
								break;
							}
						}
						None => {
							eprintln!("Failed to generate alias for {key}; skipping.",);
							continue;
						}
					};
				}
			}
			s.push_str("\n\nalias tgo=\"tg open\"");
			println!("{s}");
		}
		Commands::Server(args) => {
			server::run(config, bot_token, args.backfill_interval).await?;
		}
		Commands::Backfill => {
			backfill::backfill(&config, &bot_token).await?;
		}
		Commands::Open(args) => {
			if let Some(c) = &args.channel
				&& !config.channels.contains_key(c)
			{
				bail!("Channel not found in config")
			}

			let path = match &args.channel {
				Some(c) => chat_filepath(c),
				None => crate::server::DATA_DIR.get().unwrap().to_path_buf(),
			};
			open_with_mode(&path, OpenMode::Normal)?;
		}
	};

	Ok(())
}

pub fn chat_filepath(destination_name: &str) -> std::path::PathBuf {
	crate::server::DATA_DIR.get().unwrap().join(format!("{destination_name}.md"))
}
