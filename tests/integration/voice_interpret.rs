use crate::common;

/// A run of raw voice notes that something followed becomes one markdown block keeping every
/// note's tag; a run still being recorded stays raw; nothing is sent to Telegram.
#[test]
fn followed_run_interpreted_fresh_run_kept() {
	let mut ctx = common::TestCtx::with_config_extra("pull_interval = \"1s\"");
	let now = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
	std::fs::write(
		ctx.topic_path(),
		format!(
			"## Jan 01, 2026\n[voice] so the the first thing <!-- msg:10 01:00:00 user voice -->\n\n[voice] and the second thing <!-- msg:11 01:01:00 user voice -->\na text reply <!-- msg:12 01:02:00 user -->\n\n## {}\n[voice] still talking <!-- msg:13 {} user voice -->\n",
			now.strftime("%b %d, %Y"),
			now.strftime("%H:%M:%S")
		),
	)
	.unwrap();
	ctx.start_server();

	ctx.wait_topic(|c| c.contains("interpreted -->"));
	std::thread::sleep(std::time::Duration::from_millis(2500)); // further pulls must leave the fresh run alone

	insta::assert_snapshot!(common::redact(&ctx.read_topic()), @"
	## [DATE]
	`````md
	interpreted: retranscribed 10 | retranscribed 11
	`````
	<!-- msg:10 [TS] user voice interpreted -->
	<!-- msg:11 [TS] user voice interpreted -->
	a text reply <!-- msg:12 [TS] user -->

	## [DATE]
	[voice] still talking <!-- msg:13 [TS] user voice -->
	");
	assert_eq!(ctx.mock_sent(), "", "interpretation must not reach Telegram");
}

/// A run nothing follows is finished once it has been idle long enough.
#[test]
fn idle_trailing_run_interpreted() {
	let mut ctx = common::TestCtx::with_config_extra("pull_interval = \"1s\"");
	std::fs::write(ctx.topic_path(), "## Jan 01, 2026\n[voice] alone at the end <!-- msg:10 01:00:00 user voice -->\n").unwrap();
	ctx.start_server();

	ctx.wait_topic(|c| c.contains("interpreted -->"));
	insta::assert_snapshot!(common::redact(&ctx.read_topic()), @"
	## [DATE]
	`````md
	interpreted: retranscribed 10
	`````
	<!-- msg:10 [TS] user voice interpreted -->
	");
	assert_eq!(ctx.mock_sent(), "", "interpretation must not reach Telegram");
}

/// Transcripts from the multilingual model are interpreted as they are, without fetching the audio again.
#[test]
fn multilingual_era_run_keeps_its_transcript() {
	let mut ctx = common::TestCtx::with_config_extra("pull_interval = \"1s\"");
	std::fs::write(
		ctx.topic_path(),
		"## Dec 01, 2026\n[voice] already multilingual <!-- msg:10 01:00:00 user voice -->\na reply <!-- msg:11 01:02:00 user -->\n",
	)
	.unwrap();
	ctx.start_server();

	ctx.wait_topic(|c| c.contains("interpreted -->"));
	insta::assert_snapshot!(common::redact(&ctx.read_topic()), @"
	## [DATE]
	`````md
	interpreted: already multilingual
	`````
	<!-- msg:10 [TS] user voice interpreted -->
	a reply <!-- msg:11 [TS] user -->
	");
}
