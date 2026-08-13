use crate::common;

/// `tg send` exits 0 as soon as the message is buffered; the worker then drains it to
/// Telegram and tags it into the topic file, and the count pipe returns to 0.
#[test]
fn buffered_send_drains_to_telegram() {
	let mut ctx = common::TestCtx::new();
	ctx.start_server();

	let out = ctx.send("fire and forget");
	assert!(out.status.success(), "send failed: {}", String::from_utf8_lossy(&out.stderr));

	let content = ctx.wait_topic(|c| c.contains("fire and forget <!-- msg:"));
	assert!(ctx.mock_sent().contains("fire and forget"), "not sent to telegram: {}", ctx.mock_sent());
	assert_eq!(ctx.buffered_count(), "0", "buffer not drained; topic file:\n{content}");
}

/// The buffer endpoint rejects instantly while the Telegram head is offline (health flag is
/// maintained on failure/recovery, not computed per request), and accepts again on recovery.
#[test]
fn send_rejected_while_head_offline() {
	let mut ctx = common::TestCtx::new();
	std::fs::write(ctx.tmp.path().join("state/tg/mock_offline"), "").unwrap();
	ctx.start_server();

	let out = ctx.send("should be rejected");
	assert!(!out.status.success(), "send must fail while head is offline");
	assert!(
		String::from_utf8_lossy(&out.stderr).contains("offline"),
		"unexpected error: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	assert_eq!(ctx.buffered_count(), "0");
	assert!(!ctx.mock_sent().contains("should be rejected"));

	ctx.set_offline(false);
	let out = ctx.send("accepted after recovery");
	assert!(out.status.success(), "send failed after recovery: {}", String::from_utf8_lossy(&out.stderr));
	ctx.wait_topic(|c| c.contains("accepted after recovery <!-- msg:"));
}

/// `-i` asks Telegram to render an inlinable attachment as a photo; the caption is what lands
/// in the topic file.
#[test]
fn send_document_inline_goes_out_as_photo() {
	let mut ctx = common::TestCtx::new();
	ctx.start_server();

	let shot = ctx.write_attachment("shot.png");
	let out = ctx.send_with(&["-i", "-d", &shot], &["cap"]);
	assert!(out.status.success(), "send failed: {}", String::from_utf8_lossy(&out.stderr));

	ctx.wait_topic(|c| c.contains("cap <!-- msg:"));
	let sent = ctx.mock_sent();
	assert!(sent.contains(&format!("[photo:{shot}] cap")), "not sent as photo: {sent}");
}

/// Without `-i` the same file goes as a file attachment.
#[test]
fn send_document_without_inline_goes_out_as_document() {
	let mut ctx = common::TestCtx::new();
	ctx.start_server();

	let shot = ctx.write_attachment("shot.png");
	let out = ctx.send_with(&["-d", &shot], &["cap"]);
	assert!(out.status.success(), "send failed: {}", String::from_utf8_lossy(&out.stderr));

	ctx.wait_topic(|c| c.contains("cap <!-- msg:"));
	let sent = ctx.mock_sent();
	assert!(sent.contains(&format!("[document:{shot}] cap")), "not sent as document: {sent}");
}

/// Several documents with no message: one Telegram message each, no caption, and the topic file
/// gets the same `[photo]` placeholder an incoming media message would produce.
#[test]
fn send_multiple_documents_without_caption() {
	let mut ctx = common::TestCtx::new();
	ctx.start_server();

	let a = ctx.write_attachment("a.png");
	let b = ctx.write_attachment("b.png");
	let out = ctx.send_with(&["-i", "-d", &a, "-d", &b], &[]);
	assert!(out.status.success(), "send failed: {}", String::from_utf8_lossy(&out.stderr));

	ctx.wait_topic(|c| c.contains("[photo] <!-- msg:"));
	let sent = ctx.mock_sent();
	assert!(sent.contains(&format!("[photo:{a}] ")), "first not sent: {sent}");
	assert!(sent.contains(&format!("[photo:{b}] ")), "second not sent: {sent}");
}

/// A path that does not exist fails at the CLI, before anything is buffered.
#[test]
fn send_missing_document_fails_before_buffering() {
	let mut ctx = common::TestCtx::new();
	ctx.start_server();

	let out = ctx.send_with(&["-d", "/nonexistent/shot.png"], &["cap"]);
	assert!(!out.status.success(), "send must fail on a missing attachment");
	assert_eq!(ctx.buffered_count(), "0");
	assert!(!ctx.mock_sent().contains("cap"));
}

/// Messages accepted before a crash/died-head survive on disk and go out on startup.
#[test]
fn seeded_buffer_drains_on_startup() {
	let mut ctx = common::TestCtx::new();
	std::fs::write(
		ctx.tmp.path().join("state/tg/send_buffer.jsonl"),
		format!(
			r#"{{"Create":{{"group_id":{},"topic_id":{},"content":"survived a restart"}}}}{}"#,
			common::GROUP_ID,
			common::TOPIC_ID,
			"\n"
		),
	)
	.unwrap();

	ctx.start_server();

	ctx.wait_topic(|c| c.contains("survived a restart <!-- msg:"));
	assert!(ctx.mock_sent().contains("survived a restart"));
	assert_eq!(ctx.buffered_count(), "0");
}
