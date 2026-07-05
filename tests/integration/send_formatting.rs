use crate::common;

/// Messages sent after a long interval must be visually separated (date header / blank line +
/// dot prefix), while messages within the interval attach directly to the previous one.
#[test]
fn gap_separator_after_interval() {
	let mut ctx = common::TestCtx::new();

	std::fs::write(ctx.topic_path(), "## Jan 01, 2026\nold message <!-- msg:10 ts:1767225600 -->\n").unwrap();
	// Last write was 2 days ago → next send crosses a date boundary → date header expected
	ctx.set_last_changed(jiff::Timestamp::now().checked_sub(jiff::SignedDuration::from_hours(48)).unwrap());

	ctx.start_server();

	let out = ctx.send("second message");
	assert!(out.status.success(), "send failed: {}", String::from_utf8_lossy(&out.stderr));
	// Sent right after the previous one → no separator
	let out = ctx.send("third message");
	assert!(out.status.success(), "send failed: {}", String::from_utf8_lossy(&out.stderr));

	insta::assert_snapshot!(common::redact(&ctx.read_topic()), @"
	## [DATE]
	old message <!-- msg:10 ts:[TS] -->

	## [DATE]

	second message <!-- msg:1000 ts:[TS] -->
	third message <!-- msg:1001 ts:[TS] -->
	");
}

/// Piped multiline content (e.g. `claude_sessions | tg send ... -`) must land in the topic file
/// as a ```md block with all contents as they are. Without wrapping, the untagged lines get
/// eaten by the pull cleanup pass, leaving only the last line — the "empty" regression.
#[test]
fn multiline_message_survives_as_code_block() {
	let mut ctx = common::TestCtx::with_config_extra("pull_interval = \"1s\"");
	ctx.start_server();

	let table = "zero_fee_arb:4    empty\nnix:4 <Starship pwd disappearing>    draft\nv_utils:4    empty";
	let out = ctx.send(table);
	assert!(out.status.success(), "send failed: {}", String::from_utf8_lossy(&out.stderr));

	// Telegram receives the content verbatim
	assert!(ctx.mock_sent().contains("zero_fee_arb:4"), "content missing from mock send log: {}", ctx.mock_sent());

	// Survive at least one pull tick (which runs the tagless-line cleanup)
	std::thread::sleep(std::time::Duration::from_millis(2500));

	insta::assert_snapshot!(common::redact(&ctx.read_topic()), @"
	`````md
	zero_fee_arb:4    empty
	nix:4 <Starship pwd disappearing>    draft
	v_utils:4    empty
	`````
	<!-- msg:1000 ts:[TS] -->
	");
}
