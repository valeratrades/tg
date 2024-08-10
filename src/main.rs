use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use v_utils::io::{open_with_mode, ExpandedPath, OpenMode};
pub mod config;
mod server;
pub mod utils;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
	#[command(subcommand)]
	command: Commands,
	#[arg(long, default_value = "~/.config/tg.toml")]
	config: ExpandedPath,
}
#[derive(Subcommand)]
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
	Server,
	/// Open the messages file of a channel
	Open(OpenArgs),
}
#[derive(Args)]
struct SendArgs {
	/// Name of the channel to send the message to. Matches against the keys in the config file
	#[arg(short, long)]
	channel: String,
	/// Message to send
	message: Vec<String>,
}

#[derive(Args)]
struct OpenArgs {
	/// Name of the channel to open
	channel: String,
}

#[tokio::main]
async fn main() -> Result<()> {
	let cli = Cli::parse();
	let config = config::AppConfig::read(&cli.config.0).expect("Failed to read config file");
	//TODO!: make it possible to define the bot_token inside the config too (env overwrites if exists)
	let bot_token = std::env::var("TELEGRAM_BOT_KEY").expect("TELEGRAM_BOT_KEY not set");

	pub fn check_channel_exists(config: &config::AppConfig, channel: &str) -> Result<()> {
		if !config.channels.contains_key(channel) {
			Err(anyhow::anyhow!("Channel not found in config"))
		} else {
			Ok(())
		}
	}

	match cli.command {
		Commands::Send(args) => {
			check_channel_exists(&config, &args.channel)?;

			let message = crate::server::Message::new(args.channel, args.message.join(" "));
			let addr = format!("127.0.0.1:{}", config.localhost_port);
			let mut stream = TcpStream::connect(addr).await?;

			let json = serde_json::to_string(&message)?;
			stream.write_all(json.as_bytes()).await?;

			let mut response = [0u8; 3]; // Buffer to read "200"
			stream.read_exact(&mut response).await?;

			let _response_str = String::from_utf8_lossy(&response);
		}
		Commands::BotInfo => {
			let url = format!("https://api.telegram.org/bot{}/getMe", bot_token);
			let client = reqwest::Client::new();
			let res = client.get(&url).send().await?;

			let parsed_json: serde_json::Value = serde_json::from_str(&res.text().await?).expect("Failed to parse JSON");
			let pretty_json = serde_json::to_string_pretty(&parsed_json).expect("Failed to pretty print JSON");
			println!("{}", pretty_json);
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
			let mut s = "#!/bin/sh\n".to_string();
			for (key, _) in config.channels.iter() {
				// alias to "t{first letter of the key with which the alias is not taken}"
				let mut key_chars = key.chars();
				loop {
					match key_chars.next() {
						Some(c) => {
							let try_alias = format!("t{}", c);

							let alias_already_exists = std::process::Command::new("env")
								.arg("sh")
								.arg("-c")
								.arg(format!("which {} 2>/dev/null", try_alias))
								.output()
								.expect("Failed to execute command")
								.status
								.success();

							if alias_already_exists {
								continue;
							} else {
								s.push_str(&format!("\nalias {try_alias}=\"tg send -c {key} >/dev/null\""));
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
			println!("{s}");
		}
		Commands::Server => {
			server::run(config, bot_token, cli.config.as_ref()).await?;
		}
		Commands::Open(args) => {
			check_channel_exists(&config, &args.channel)?;

			let path = chat_filepath(&args.channel);
			open_with_mode(&path, OpenMode::Pager)?;
		}
	};

	Ok(())
}

pub fn chat_filepath(destination_name: &str) -> std::path::PathBuf {
	crate::server::VAR_DIR.join(format!("{destination_name}.md"))
}
