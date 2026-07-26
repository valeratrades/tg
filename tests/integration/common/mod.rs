//! Harness: runs the real `tg` binary (server in `--mock` mode + CLI subprocesses) against
//! a throwaway XDG tree, following https://matklad.github.io/2021/05/31/how-to-test.html —
//! tests observe only user-visible state: topic files, mock_sent.jsonl, exit codes.
use std::{
	io::{Read as _, Write as _},
	path::PathBuf,
	process::{Child, Command, Output, Stdio},
	time::Duration,
};

pub const GROUP_ID: u64 = 1000000001;
pub const TOPIC_ID: u64 = 2;

pub struct TestCtx {
	pub tmp: tempfile::TempDir,
	pub port: u16,
	server: Option<Child>,
}

impl TestCtx {
	pub fn new() -> Self {
		Self::with_config_extra("")
	}

	/// `extra` is appended to the top-level section of tg.toml (e.g. `pull_interval = "1s"`)
	pub fn with_config_extra(extra: &str) -> Self {
		let tmp = tempfile::tempdir().unwrap();
		for d in ["config", "state", "data", "cache"] {
			std::fs::create_dir_all(tmp.path().join(d)).unwrap();
		}

		let port = free_port();
		std::fs::write(
			tmp.path().join("config/tg.toml"),
			format!(
				r#"localhost_port = {port}
max_messages_per_chat = 100
{extra}

[groups]
test = "-100{GROUP_ID}/{TOPIC_ID}"
"#
			),
		)
		.unwrap();

		let state_tg = tmp.path().join("state/tg");
		std::fs::create_dir_all(state_tg.join("testgroup")).unwrap();
		std::fs::write(
			state_tg.join("topics_metadata.json"),
			format!(r#"{{"groups":{{"{GROUP_ID}":{{"name":"testgroup","topics":{{"{TOPIC_ID}":"general"}}}}}}}}"#),
		)
		.unwrap();

		Self { tmp, port, server: None }
	}

	pub fn cmd(&self) -> Command {
		let mut c = Command::new(bin());
		c.env("XDG_CONFIG_HOME", self.tmp.path().join("config"))
			.env("XDG_STATE_HOME", self.tmp.path().join("state"))
			.env("XDG_DATA_HOME", self.tmp.path().join("data"))
			.env("XDG_CACHE_HOME", self.tmp.path().join("cache"))
			.env("HOME", self.tmp.path());
		c
	}

	pub fn start_server(&mut self) {
		assert!(self.server.is_none(), "server already running");
		let child = self.cmd().args(["server", "--mock"]).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
		self.server = Some(child);

		for _ in 0..100 {
			if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", self.port)) {
				s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
				if s.write_all(br#"{"type":"Ping"}"#).is_ok() {
					let mut buf = [0u8; 1024];
					if let Ok(n) = s.read(&mut buf)
						&& n > 0 && String::from_utf8_lossy(&buf[..n]).contains(r#""success":true"#)
					{
						return;
					}
				}
			}
			std::thread::sleep(Duration::from_millis(50));
		}
		panic!("server did not become ready:\n{}", self.stop_server());
	}

	/// Kill the server and return its stderr (for diagnostics)
	pub fn stop_server(&mut self) -> String {
		let Some(mut child) = self.server.take() else { return String::new() };
		child.kill().unwrap();
		let mut stderr = String::new();
		if let Some(mut pipe) = child.stderr.take() {
			pipe.read_to_string(&mut stderr).unwrap();
		}
		child.wait().unwrap();
		stderr
	}

	pub fn send(&self, msg: &str) -> Output {
		self.cmd().args(["send", "-g", &GROUP_ID.to_string(), "-t", &TOPIC_ID.to_string(), msg]).output().unwrap()
	}

	pub fn delete(&self, msg_id: i32) -> Output {
		self.cmd()
			.args(["schedule-update", "delete", &GROUP_ID.to_string(), &TOPIC_ID.to_string(), &msg_id.to_string()])
			.output()
			.unwrap()
	}

	pub fn topic_path(&self) -> PathBuf {
		self.tmp.path().join("state/tg/testgroup/general.md")
	}

	pub fn read_topic(&self) -> String {
		std::fs::read_to_string(self.topic_path()).unwrap_or_default()
	}

	pub fn set_last_changed(&self, ts: jiff::Timestamp) {
		use xattr::FileExt as _;
		let f = std::fs::File::open(self.topic_path()).unwrap();
		f.set_xattr("user.last_changed", ts.to_string().as_bytes()).unwrap();
	}

	pub fn mock_sent(&self) -> String {
		std::fs::read_to_string(self.tmp.path().join("state/tg/mock_sent.jsonl")).unwrap_or_default()
	}

	pub fn buffered_count(&self) -> String {
		std::fs::read_to_string(self.tmp.path().join("state/tg/buffered_count")).unwrap_or_default().trim().to_string()
	}

	/// Flip the mock server's simulated Telegram connectivity (watched at 50ms granularity)
	pub fn set_offline(&self, offline: bool) {
		let flag = self.tmp.path().join("state/tg/mock_offline");
		if offline {
			std::fs::write(&flag, "").unwrap();
		} else {
			std::fs::remove_file(&flag).unwrap();
		}
		std::thread::sleep(Duration::from_millis(150)); // > watcher interval
	}

	/// Poll until `pred` holds on the topic file; panics with the last content on timeout
	pub fn wait_topic(&self, pred: impl Fn(&str) -> bool) -> String {
		for _ in 0..100 {
			let content = self.read_topic();
			if pred(&content) {
				return content;
			}
			std::thread::sleep(Duration::from_millis(50));
		}
		panic!("timed out waiting on topic file, last content:\n{}", self.read_topic());
	}
}

impl Drop for TestCtx {
	fn drop(&mut self) {
		self.stop_server();
	}
}

/// Volatile values → stable placeholders so insta snapshots hold across runs
pub fn redact(s: &str) -> String {
	let s = regex::Regex::new(r"ts:\d+").unwrap().replace_all(s, "ts:[TS]");
	let s = regex::Regex::new(r"\d{2}:\d{2}:\d{2}").unwrap().replace_all(&s, "[TS]");
	let s = regex::Regex::new(r"## [A-Za-z]{3} \d{2}, \d{4}").unwrap().replace_all(&s, "## [DATE]");
	s.into_owned()
}

fn bin() -> PathBuf {
	let mut path = std::env::current_exe().unwrap();
	path.pop(); // test binary name
	path.pop(); // deps/
	path.push("tg");
	path
}

fn free_port() -> u16 {
	std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}
