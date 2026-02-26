use eyre::{Result, bail};
use jiff::civil::Date;
use v_utils::trades::Timeframe;

use crate::{
	config::LiveSettings,
	last::{confirm_large_output, group_consecutive, load_all_messages, print_message_groups},
};

pub async fn run(datetime: Option<String>, back: Option<Timeframe>, config: &LiveSettings) -> Result<()> {
	let today = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date();

	let cutoff_date = match (datetime, back) {
		(Some(_), Some(_)) => bail!("Cannot specify both a datetime and --back"),
		(None, None) => bail!("Must specify either a datetime or --back"),
		(None, Some(tf)) => {
			let span = jiff::Span::try_from(tf.duration())?;
			today.checked_sub(span)?
		}
		(Some(dt_str), None) => parse_partial_date(&dt_str, today)?,
	};

	let mut messages = load_all_messages();
	messages.retain(|m| m.date >= cutoff_date);
	messages.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.message_id.cmp(&b.message_id)));

	if !confirm_large_output(messages.len(), config)? {
		return Ok(());
	}

	print_message_groups(&group_consecutive(&messages));

	Ok(())
}

/// Parse a partial date string, filling missing components from `now`.
/// Supported formats: "YYYY-MM-DD", "MM-DD", "DD"
/// Also accepts `/` as separator.
fn parse_partial_date(s: &str, today: Date) -> Result<Date> {
	let s = s.replace('/', "-");
	let parts: Vec<&str> = s.split('-').collect();
	match parts.len() {
		3 => {
			let year: i16 = parts[0].parse()?;
			let month: i8 = parts[1].parse()?;
			let day: i8 = parts[2].parse()?;
			Ok(Date::new(year, month, day)?)
		}
		2 => {
			let month: i8 = parts[0].parse()?;
			let day: i8 = parts[1].parse()?;
			let candidate = Date::new(today.year(), month, day)?;
			if candidate > today { Ok(Date::new(today.year() - 1, month, day)?) } else { Ok(candidate) }
		}
		1 => {
			let day: i8 = parts[0].parse()?;
			let candidate = Date::new(today.year(), today.month(), day)?;
			if candidate > today {
				let prev = today.first_of_month().checked_sub(jiff::Span::new().days(1))?;
				Ok(Date::new(prev.year(), prev.month(), day)?)
			} else {
				Ok(candidate)
			}
		}
		_ => bail!("Invalid date format: '{s}'. Expected YYYY-MM-DD, MM-DD, or DD"),
	}
}
