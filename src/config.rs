// so snapshot tests work
use std::{
	collections::BTreeMap,
	path::{Path, PathBuf},
};

pub use tg::chat::TelegramDestination;
use tracing::{debug, error, info, instrument, warn};
use v_utils::macros::MyConfigPrimitives;

use crate::server::DATA_DIR;

#[derive(Debug, Default, derive_new::new, Clone, MyConfigPrimitives)]
pub struct AppConfig {
	pub channels: BTreeMap<String, TelegramDestination>,
	pub localhost_port: u16,
}

#[instrument(skip(config), fields(destination = ?destination))]
pub fn display_destination(destination: &TelegramDestination, config: &AppConfig) -> String {
	match config.channels.iter().find(|(_, td)| *td == destination) {
		Some((key, _)) => {
			debug!("Found named channel: {}", key);
			key.clone()
		}
		None => {
			let name = match destination {
				TelegramDestination::ChannelExactUid(id) => format!("{}", id),
				TelegramDestination::ChannelUsername(username) => username.clone(),
				TelegramDestination::Group { id, thread_id } => format!("{}_slash_{}", id, thread_id),
			};
			debug!("Using generated name for destination: {}", name);
			name
		}
	}
}

impl AppConfig {
	#[instrument(fields(config_path = %path.display()))]
	pub fn read(path: &Path) -> Result<Self, config::ConfigError> {
		info!("Reading config from: {}", path.display());

		if !path.exists() {
			error!("Config file does not exist at path: {}", path.display());
			return Err(config::ConfigError::Message(format!("Config file not found: {}", path.display())));
		}

		debug!("Building config from file");
		let builder = config::Config::builder().add_source(config::File::with_name(&format!("{}", path.display())));

		let settings: config::Config = match builder.build() {
			Ok(s) => {
				debug!("Successfully built config");
				s
			}
			Err(e) => {
				error!("Failed to build config: {}", e);
				return Err(e);
			}
		};

		let settings: Self = match settings.try_deserialize::<Self>() {
			Ok(s) => {
				info!("Successfully deserialized config with {} channels", s.channels.len());
				debug!("Channels: {:?}", s.channels.keys().collect::<Vec<_>>());
				s
			}
			Err(e) => {
				error!("Failed to deserialize config: {}", e);
				return Err(e);
			}
		};

		let data_dir_path = std::env::var("XDG_DATA_HOME")
			.map(PathBuf::from)
			.unwrap_or_else(|_| {
				warn!("XDG_DATA_HOME not set, this will panic");
				PathBuf::from("") // This will cause unwrap to panic
			})
			.join("tg");

		info!("Initializing data directory: {}", data_dir_path.display());
		let data_dir = DATA_DIR.get_or_init(|| data_dir_path);

		match std::fs::create_dir_all(data_dir) {
			Ok(_) => {
				info!("Data directory ready: {}", data_dir.display());
			}
			Err(e) => {
				error!("Failed to create data directory '{}': {}", data_dir.display(), e);
				return Err(config::ConfigError::Foreign(Box::new(e)));
			}
		}

		Ok(settings)
	}
}
