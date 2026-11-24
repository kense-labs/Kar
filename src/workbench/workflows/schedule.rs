use crate::{Error, Result};
use jiff::{Timestamp, civil::Time, tz::TimeZone};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Schedule {
    Manual,
    Once {
        at_ms: i64,
    },
    Interval {
        every_ms: i64,
    },
    FixedTime {
        time: String,
        #[serde(default)]
        weekdays: Vec<i8>,
        /// Omitted uses the server's local zone.
        zone: Option<String>,
    },
    Webhook,
}

fn fixed_time(time: &str, weekdays: &[i8]) -> Result<Time> {
    let bad = || Error::Refused("invalid fixed time or weekday".into());
    let (h, m) = time.trim().split_once(':').ok_or_else(bad)?;
    if h.is_empty()
        || h.len() > 2
        || m.len() != 2
        || !h.bytes().chain(m.bytes()).all(|b| b.is_ascii_digit())
        || weekdays.iter().any(|d| !(0..=6).contains(d))
    {
        return Err(bad());
    }
    Time::new(
        h.parse().map_err(|_| bad())?,
        m.parse().map_err(|_| bad())?,
        0,
        0,
    )
    .map_err(|error| Error::Refused(error.to_string()))
}

impl Schedule {
    pub(super) fn same_timing(&self, other: &Self) -> bool {
        let (
            Self::FixedTime {
                time,
                weekdays,
                zone,
            },
            Self::FixedTime {
                time: other_time,
                weekdays: other_days,
                zone: other_zone,
            },
        ) = (self, other)
        else {
            return self == other;
        };
        let days = |set: &[i8], day| set.is_empty() || set.contains(&day);
        zone == other_zone
            && fixed_time(time, weekdays).ok() == fixed_time(other_time, other_days).ok()
            && (0..=6).all(|d| days(weekdays, d) == days(other_days, d))
    }

    pub fn next_after(&self, from_ms: i64) -> Result<Option<i64>> {
        let invalid = |e: jiff::Error| Error::Refused(e.to_string());
        match self {
            Self::Manual | Self::Webhook => Ok(None),
            Self::Once { at_ms } => Ok((*at_ms > from_ms).then_some(*at_ms)),
            Self::Interval { every_ms } if *every_ms >= 60_000 => {
                Ok(from_ms.checked_add(*every_ms))
            }
            Self::Interval { .. } => Err(Error::Refused(
                "interval must be at least one minute".into(),
            )),
            Self::FixedTime {
                time,
                weekdays,
                zone,
            } => {
                let at = fixed_time(time, weekdays)?;
                let zone = match zone {
                    Some(name) => TimeZone::get(name).map_err(invalid)?,
                    None => TimeZone::system(),
                };
                let mut date = Timestamp::from_millisecond(from_ms)
                    .map_err(invalid)?
                    .to_zoned(zone.clone())
                    .date();
                for _ in 0..=7 {
                    if weekdays.is_empty()
                        || weekdays.contains(&date.weekday().to_sunday_zero_offset())
                    {
                        let next = date
                            .to_datetime(at)
                            .to_zoned(zone.clone())
                            .map_err(invalid)?
                            .timestamp()
                            .as_millisecond();
                        if next > from_ms {
                            return Ok(Some(next));
                        }
                    }
                    date = date.tomorrow().map_err(invalid)?;
                }
                Ok(None)
            }
        }
    }
    pub(super) fn missed(&self, due: i64, now: i64) -> bool {
        matches!(self, Self::FixedTime { .. }) && now.saturating_sub(due) > 600_000
    }
}
