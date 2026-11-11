use chrono::{Datelike, Duration, LocalResult, NaiveDate, TimeZone, Utc};

use crate::TemporalSource;
use crate::model::{AbsoluteTime, Episode, EpisodeStatus, RelativeDuration};

pub(super) fn extraction_submission_limit(transcript_turns: usize) -> usize {
    transcript_turns.div_ceil(5).clamp(10, 40)
}

/// Resolve model-normalized time. Ordinary-message timestamps remain hidden. Task lifecycle
/// timestamps are visible and validated separately, but the exact structured source value
/// remains authoritative here.
pub(super) fn resolve_episode(episode: &mut Episode, sources: &[TemporalSource], timezone: &str) {
    let Some(source) = sources
        .iter()
        .find(|source| source.handle == episode.anchor.message)
    else {
        tracing::debug!(
            message = %episode.anchor.message,
            "pkm episodic anchor unresolved"
        );
        return;
    };
    if episode.anchor.quote.trim().is_empty() {
        if let Some(event_at) = source.task_event_at {
            episode.resolved_start = match episode.status {
                EpisodeStatus::Planned => source.task_target_at,
                EpisodeStatus::Occurred | EpisodeStatus::Cancelled | EpisodeStatus::Unconfirmed => {
                    Some(event_at)
                }
            };
        }
        return;
    }
    let Ok(tz) = timezone.parse::<chrono_tz::Tz>() else {
        tracing::debug!(%timezone, "pkm episodic timezone invalid");
        return;
    };
    let anchor = source.created_at.with_timezone(&tz);
    let resolved = if let Some(duration) = &episode.duration {
        resolve_duration(anchor, duration)
    } else if let Some(absolute) = &episode.absolute {
        resolve_absolute(anchor, absolute, tz)
    } else {
        None
    };
    if let Some((start, end)) = resolved {
        episode.resolved_start = Some(start);
        episode.resolved_end = end;
    }
    tracing::debug!(
        anchor_text = %episode.anchor.quote,
        duration = ?episode.duration,
        absolute = ?episode.absolute,
        resolved_start = ?episode.resolved_start,
        resolved_end = ?episode.resolved_end,
        "pkm episodic time grounded"
    );
}

pub(super) fn resolve_duration(
    anchor: chrono::DateTime<chrono_tz::Tz>,
    value: &RelativeDuration,
) -> Option<(chrono::DateTime<Utc>, Option<chrono::DateTime<Utc>>)> {
    use crate::model::{TemporalDirection::*, TemporalSemantics, TemporalUnit::*};
    let sign: i64 = match value.direction {
        Past => -1,
        Present => 0,
        Future => 1,
    };
    let amount = i64::from(value.amount);
    if value.amount == 0 {
        return None;
    }
    if value.semantics == TemporalSemantics::Elapsed {
        let delta = match value.unit {
            Minute => Duration::minutes(amount),
            Hour => Duration::hours(amount),
            Day => Duration::days(amount),
            Week => Duration::weeks(amount),
            Month | Year => return None,
        };
        return Some(((anchor + delta * sign as i32).with_timezone(&Utc), None));
    }

    let date = anchor.date_naive();
    let start_date = match value.unit {
        Week => {
            let monday = date - Duration::days(i64::from(date.weekday().num_days_from_monday()));
            monday + Duration::weeks(sign * amount)
        }
        Day => date + Duration::days(sign * amount),
        Month => {
            let index = i64::from(date.year()) * 12 + i64::from(date.month0()) + sign * amount;
            NaiveDate::from_ymd_opt(
                (index.div_euclid(12)) as i32,
                (index.rem_euclid(12) + 1) as u32,
                1,
            )?
        }
        Year => NaiveDate::from_ymd_opt(date.year() + (sign * amount) as i32, 1, 1)?,
        Hour | Minute => {
            let delta = if value.unit == Hour {
                Duration::hours(amount)
            } else {
                Duration::minutes(amount)
            };
            return Some(((anchor + delta * sign as i32).with_timezone(&Utc), None));
        }
    };
    let start = match anchor
        .timezone()
        .from_local_datetime(&start_date.and_hms_opt(0, 0, 0)?)
    {
        LocalResult::Single(value) => value,
        _ => return None,
    };
    let end = match value.unit {
        Week => start + Duration::weeks(1),
        Day => start + Duration::days(1),
        Month => start.checked_add_months(chrono::Months::new(1))?,
        Year => start.checked_add_months(chrono::Months::new(12))?,
        Hour | Minute => unreachable!(),
    };
    Some((start.with_timezone(&Utc), Some(end.with_timezone(&Utc))))
}

pub(super) fn resolve_absolute(
    anchor: chrono::DateTime<chrono_tz::Tz>,
    value: &AbsoluteTime,
    tz: chrono_tz::Tz,
) -> Option<(chrono::DateTime<Utc>, Option<chrono::DateTime<Utc>>)> {
    let date = NaiveDate::from_ymd_opt(
        value.year.unwrap_or(anchor.year()),
        value.month.unwrap_or(anchor.month()),
        value.day.unwrap_or(anchor.day()),
    )?;
    let local = date.and_hms_opt(value.hour.unwrap_or(0), value.minute.unwrap_or(0), 0)?;
    match tz.from_local_datetime(&local) {
        LocalResult::Single(value) => Some((value.with_timezone(&Utc), None)),
        _ => None,
    }
}
