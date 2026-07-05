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
