use clap::{Args, Parser, Subcommand};
use std::env;
use v_utils::io::ExpandedPath;
use anyhow::Result;
mod config;

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
}
#[derive(Args)]
struct SendArgs {
	/// Name of the channel to send the message to. Matches against the keys in the config file
	#[arg(short, long)]
	channel: String,

	/// Message to send
	message: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
	let cli = Cli::parse();
	let config = config::AppConfig::read(&cli.config.0).expect("Failed to read config file");
	//TODO!: make it possible to define the bot_token inside the config too (env overwrites if exists)
	let bot_token = env::var("TELEGRAM_BOT_KEY").expect("TELEGRAM_BOT_KEY not set");

	match cli.command {
		Commands::Send(args) => {
			let destination = match config.channels.get(&args.channel) {
				Some(d) => d,
				None => {
					return Err(anyhow::anyhow!("Channel not found in {}", cli.config.display()));
				}
			};

			let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
			let mut params = vec![("text", args.message.join(" "))];
			params.extend(destination.destination_params());
			let client = reqwest::Client::new();
			let res = client.post(&url).form(&params).send().await?;

			println!("{:#?}\nSender: {bot_token}\n{:#?}", res.text().await?, destination);
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
	};

	Ok(())
}
