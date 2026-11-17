//! `tg save-file` goes through the server's session to Saved Messages.
use crate::common;

#[test]
fn save_file_goes_to_saved_messages() {
	let mut ctx = common::TestCtx::new();
	ctx.start_server();

	let a = ctx.write_attachment("a.pdf");
	let b = ctx.write_attachment("b.png");
	let out = ctx.cmd().args(["save-file", &a, &b]).output().unwrap();
	assert!(out.status.success(), "save-file failed: {}", String::from_utf8_lossy(&out.stderr));

	let sent = ctx.mock_sent();
	assert!(sent.contains(&format!(r#""op":"save","path":"{a}""#)), "a not saved: {sent}");
	assert!(sent.contains(&format!(r#""op":"save","path":"{b}""#)), "b not saved: {sent}");
	assert!(ctx.read_topic().is_empty(), "saved files leaked into a topic file");
}

#[test]
fn save_file_without_server_sends_nothing() {
	let ctx = common::TestCtx::new();
	let a = ctx.write_attachment("a.pdf");
	let out = ctx.cmd().args(["save-file", &a]).output().unwrap();
	assert!(!out.status.success());
	assert!(ctx.mock_sent().is_empty());
}
