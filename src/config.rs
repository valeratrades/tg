// so snapshot tests work
use std::{
	collections::BTreeMap,
	path::{Path, PathBuf},
};

pub use tg::chat::TelegramDestination;
use v_utils::macros::MyConfigPrimitives;

use crate::server::DATA_DIR;

#[derive(Debug, Default, derive_new::new, Clone, MyConfigPrimitives)]
pub struct AppConfig {
	pub channels: BTreeMap<String, TelegramDestination>,
	pub localhost_port: u16,
}

pub fn display_destination(destination: &TelegramDestination, config: &AppConfig) -> String {
	match config.channels.iter().find(|(_, &td)| td == *destination) {
		Some((key, _)) => key.clone(),
		None => match destination {
			TelegramDestination::Channel(id) => format!("{}", id),
			TelegramDestination::Group { id, thread_id } => format!("{}_slash_{}", id, thread_id),
		},
	}
}

impl AppConfig {
	pub fn read(path: &Path) -> Result<Self, config::ConfigError> {
		let builder = config::Config::builder().add_source(config::File::with_name(&format!("{}", path.display())));

		let settings: config::Config = builder.build()?;
		let settings: Self = settings.try_deserialize()?;

		let data_dir = DATA_DIR.get_or_init(|| std::env::var("XDG_DATA_HOME").map(PathBuf::from).unwrap().join("tg"));
		std::fs::create_dir_all(data_dir).map_err(|e| config::ConfigError::Foreign(Box::new(e)))?;

		Ok(settings)
	}
}
