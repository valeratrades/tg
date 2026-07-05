use std::{
	path::PathBuf,
	sync::{Mutex, MutexGuard},
};

use eyre::Result;
use tracing::info;

use crate::sync::MessageUpdate;

/// Disk-backed queue of accepted-but-not-yet-sent messages.
///
/// `tg send` returns as soon as its message lands here; the server worker drains it to Telegram.
/// Persistence means a died sync head (or server restart) never loses accepted messages.
/// `buffered_count` mirrors the queue length for the status bar to poll.
pub struct SendBuffer {
	path: PathBuf,
	count_path: PathBuf,
	dead_letter_path: PathBuf,
	lock: Mutex<()>,
}

impl SendBuffer {
	pub fn load() -> Result<Self> {
		let data_dir = crate::server::DATA_DIR.get().expect("init_data_dir ran in main");
		let s = Self {
			path: data_dir.join("send_buffer.jsonl"),
			count_path: data_dir.join("buffered_count"),
			dead_letter_path: data_dir.join("dead_letter.jsonl"),
			lock: Mutex::new(()),
		};
		{
			let guard = s.lock.lock().unwrap();
			s.write_count(&guard)?;
		}
		Ok(s)
	}

	/// Append updates; returns the new queue length.
	pub fn append(&self, updates: &[MessageUpdate]) -> Result<usize> {
		use std::io::Write as _;
		let guard = self.lock.lock().unwrap();
		let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
		for u in updates {
			writeln!(f, "{}", serde_json::to_string(u)?)?;
		}
		drop(f);
		self.write_count(&guard)
	}

	/// Read the current queue without consuming it. Entries are only removed via `pop_first`
	/// after the drain attempt, so a crash mid-send keeps them queued.
	pub fn snapshot(&self) -> Result<Vec<MessageUpdate>> {
		let _guard = self.lock.lock().unwrap();
		self.read_entries()
	}

	/// Drop the first `n` entries (the drained prefix; appends only ever grow the tail).
	pub fn pop_first(&self, n: usize) -> Result<usize> {
		let guard = self.lock.lock().unwrap();
		let content = std::fs::read_to_string(&self.path).unwrap_or_default();
		let rest: Vec<&str> = content.lines().skip(n).collect();
		let new_content = if rest.is_empty() { String::new() } else { format!("{}\n", rest.join("\n")) };
		std::fs::write(&self.path, new_content)?;
		self.write_count(&guard)
	}

	/// A message Telegram definitively rejected: retrying is a poison loop, dropping loses data.
	/// Park it on disk and move on.
	pub fn dead_letter(&self, update: &MessageUpdate, error: &str) -> Result<()> {
		use std::io::Write as _;
		let _guard = self.lock.lock().unwrap();
		let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.dead_letter_path)?;
		writeln!(f, "{}", serde_json::json!({"update": update, "error": error}))?;
		info!("Dead-lettered rejected message: {error}");
		Ok(())
	}

	fn read_entries(&self) -> Result<Vec<MessageUpdate>> {
		let content = std::fs::read_to_string(&self.path).unwrap_or_default();
		content.lines().map(|l| Ok(serde_json::from_str(l)?)).collect()
	}

	fn write_count(&self, _proof_of_lock: &MutexGuard<'_, ()>) -> Result<usize> {
		let n = std::fs::read_to_string(&self.path).map(|s| s.lines().count()).unwrap_or(0);
		std::fs::write(&self.count_path, format!("{n}\n"))?;
		Ok(n)
	}
}
