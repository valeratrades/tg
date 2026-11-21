//! Rewrites finished runs of raw voice transcripts into readable markdown, in place.
use std::{collections::HashMap, path::Path};

use eyre::{Result, bail};
use grammers_client::Client;
use grammers_tl_types as tl;
use jiff::{SignedDuration, Timestamp, civil::Date, tz::TimeZone};
use tracing::{info, warn};

use crate::sync::{MSG_TAG_RE, extract_time, has_qualifier, parse_file_messages};

/// Silence after the last note of a run nothing followed; spans the recording length of a next note, hence not shorter.
const IDLE: SignedDuration = SignedDuration::from_mins(10);
const CONTEXT_MESSAGES: usize = 20;
/// Transcripts before this came from an English-only whisper model, which dropped or invented speech in other languages.
const MULTILINGUAL_SINCE: Timestamp = Timestamp::constant(1795305600, 0); // 2026-11-22T00:00:00Z
const PROMPT: &str = "\
You receive speech-to-text transcripts of one person thinking aloud, possibly spread over several consecutive voice notes. \
Rewrite them into readable markdown. You fix the representation, never the ideas.

Do:
- fix misheard words and names where the topic or the preceding messages make the intended one clear
- drop filler, false starts, restarts, and repetition loops the transcriber hallucinated
- merge the notes into one text: note boundaries carry no meaning
- add headings (`###` and deeper), bullets or numbered steps only where the content itself has that structure

Never:
- add ideas, conclusions, advice or a summary
- drop or soften a claim, number, plan, doubt or open question
- change the person (keep the speaker's first person) or the tone

Write in English, translating whatever was said in another language.
A guessed term or a fragment you cannot recover: `[?: what was heard → best guess]`.
Output the markdown only: no preamble, no code fence around it.";

pub(crate) enum Interpreter<'a> {
	Llm {
		llm: &'a ask_llm::Client,
		telegram: &'a Client,
		peer: &'a tl::enums::InputPeer,
	},
	/// Deterministic, so tests exercise everything but the model and the audio.
	Mock,
}
impl Interpreter<'_> {
	async fn retranscribe(&self, ids: &[i32]) -> Result<HashMap<i32, String>> {
		match self {
			Self::Llm { telegram, peer, .. } => crate::pull::transcribe_voices(telegram, peer, ids).await,
			Self::Mock => Ok(ids.iter().map(|id| (*id, format!("retranscribed {id}"))).collect()),
		}
	}

	async fn interpret(&self, topic: &str, context: &[String], raws: &[&str]) -> Result<String> {
		let text = match self {
			Self::Llm { llm, .. } => {
				let context = context.iter().map(|c| format!("<message>{c}</message>")).collect::<Vec<_>>().join("\n");
				let notes = raws.iter().map(|r| format!("<note>{r}</note>")).collect::<Vec<_>>().join("\n");
				let mut conv = ask_llm::Conversation::new_with_system(PROMPT);
				conv.add(
					ask_llm::Role::User,
					format!("<topic>{topic}</topic>\n<preceding_messages>\n{context}\n</preceding_messages>\n<voice_notes>\n{notes}\n</voice_notes>"),
				);
				llm.conversation(&conv).await?.text
			}
			Self::Mock => format!("interpreted: {}", raws.join(" | ")),
		};
		if text.trim().is_empty() {
			bail!("model returned nothing");
		}
		Ok(text.trim().to_string())
	}
}

/// Interprets the first finished run of raw `user voice` notes in the file.
/// Returns whether a model call was made, successful or not.
pub(crate) async fn interpret_voice_runs(path: &Path, topic: &str, interpreter: &Interpreter<'_>) -> Result<bool> {
	let content = std::fs::read_to_string(path)?;
	let lines: Vec<&str> = content.lines().collect();
	let Some(run) = first_finished_run(&lines, Timestamp::now()) else { return Ok(false) };
	let (first, last) = (&run[0], &run[run.len() - 1]);

	let preceding: Vec<String> = parse_file_messages(&lines[..first.start].join("\n"))
		.into_values()
		.map(|m| m.content)
		.filter(|c| !c.is_empty())
		.collect();
	let context = &preceding[preceding.len().saturating_sub(CONTEXT_MESSAGES)..];
	let stale: Vec<i32> = run.iter().filter(|n| n.failed || n.at.is_none_or(|t| t < MULTILINGUAL_SINCE)).map(|n| n.id).collect();
	let retranscribed = match stale.is_empty() {
		true => HashMap::new(),
		false => match interpreter.retranscribe(&stale).await {
			Ok(r) => r,
			Err(e) => {
				warn!(path = %path.display(), error = %e, "Voice re-transcription failed; retried next pull");
				return Ok(true);
			}
		},
	};
	if let Some(lost) = stale.iter().find(|id| !retranscribed.contains_key(id)) {
		warn!(path = %path.display(), msg_id = lost, "Voice note's audio is gone from Telegram; interpreting its old transcript");
	}
	let raws: Vec<&str> = run
		.iter()
		.map(|n| match retranscribed.get(&n.id) {
			Some(t) => t.as_str(),
			None => n.raw.as_str(), // audio no longer on Telegram: the old transcript is all there is
		})
		.collect();
	let interpreted = match interpreter.interpret(topic, context, &raws).await {
		Ok(t) => t,
		Err(e) => {
			warn!(path = %path.display(), error = %e, "Voice interpretation failed; raw kept, retried next pull");
			return Ok(true);
		}
	};

	let fence = "`".repeat(crate::server::max_backtick_run(&interpreted).max(4) + 1);
	let tags = run.iter().map(|n| n.tag.replacen(" -->", " interpreted -->", 1)).collect::<Vec<_>>().join("\n");
	let block = format!("{fence}md\n{interpreted}\n{fence}\n{tags}");

	let span = lines[first.start..=last.end].join("\n");
	let fresh = std::fs::read_to_string(path)?; // the file may have been edited while the model ran
	if fresh.matches(&span).count() != 1 {
		warn!(path = %path.display(), "Voice run changed during interpretation; retried next pull");
		return Ok(true);
	}
	std::fs::write(path, fresh.replacen(&span, &block, 1))?;
	info!(path = %path.display(), notes = run.len(), retranscribed = retranscribed.len(), "Interpreted voice run");
	Ok(true)
}

struct Note {
	id: i32,
	start: usize,
	/// Line of the tag, inclusive.
	end: usize,
	raw: String,
	tag: String,
	at: Option<Timestamp>,
	failed: bool,
}

/// A run is consecutive raw `user voice` notes with only blank lines between them; it is finished once anything follows it, or after [`IDLE`].
fn first_finished_run(lines: &[&str], now: Timestamp) -> Option<Vec<Note>> {
	let mut run: Vec<Note> = Vec::new();
	let mut date: Option<Date> = None;
	let mut i = 0;
	while i < lines.len() {
		let trimmed = lines[i].trim();
		if trimmed.is_empty() {
			i += 1;
			continue;
		}
		let start = i;
		if let Ok(d) = Date::strptime("## %b %d, %Y", trimmed) {
			date = Some(d);
		}
		let unit = if trimmed.starts_with("`````") && trimmed.contains("md") {
			let close = (i + 1..lines.len()).find(|&j| lines[j].contains("`````"));
			close.and_then(|c| {
				let closing = lines[c].trim();
				let tag_line = if closing.chars().all(|ch| ch == '`') { c + 1 } else { c };
				let caps = MSG_TAG_RE.captures(lines.get(tag_line)?)?;
				Some((tag_line, caps, lines[i + 1..c].join("\n")))
			})
		} else {
			MSG_TAG_RE.captures(lines[i]).map(|caps| {
				let body = MSG_TAG_RE.replace(lines[i], "").trim().to_string();
				(i, caps, body.strip_prefix(". ").map(str::to_string).unwrap_or(body))
			})
		};

		let note = unit.as_ref().and_then(|(end, caps, body)| {
			let raw = body.strip_prefix("[voice] ")?;
			let eligible = has_qualifier(caps, "user") && has_qualifier(caps, "voice") && !has_qualifier(caps, "interpreted") && !caps[0].starts_with("<!-- forwarded");
			eligible.then(|| Note {
				id: caps[1].parse().expect("regex-matched digits"),
				start,
				end: *end,
				raw: raw.trim().to_string(),
				tag: caps[0].to_string(),
				at: date
					.zip(extract_time(caps))
					.map(|(d, t)| d.to_datetime(t).to_zoned(TimeZone::UTC).expect("UTC has no gaps").timestamp()),
				failed: raw.starts_with("[transcription failed"),
			})
		});
		match note {
			Some(n) => run.push(n),
			None if !run.is_empty() => return Some(run),
			None => {}
		}
		i = unit.map_or(i, |(end, ..)| end) + 1;
	}

	let at = run.last()?.at?; // unknown age (no date header before it, or a legacy tag without time): only a following message finishes it
	(now.duration_since(at) >= IDLE).then_some(run)
}
