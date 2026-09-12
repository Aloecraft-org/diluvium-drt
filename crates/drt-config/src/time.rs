//! RFC 3339 instants, as much of them as this crate compares.
//!
//! Hand-written rather than `chrono` or `time`, because the only questions
//! asked of a timestamp here are "is this instant in the future" and "do
//! these two windows overlap" (consent.md §7), and the dependency budget
//! dollup was quoted has four crates in it. The **caller** supplies "now",
//! for the same reason it supplies entropy in [`crate::id`]: a crate shared
//! by a runtime that reads its clock through `drt-platform` and a CLI that
//! reads the host's must not pick one.
//!
//! ## surface block
//!
//! - Entry points: [`Timestamp::parse`], text to an instant;
//!   [`Timestamp::from_unix_secs`]; [`Timestamp::is_after`], the comparison
//!   §7 step 4 makes; [`Window`] and [`Window::intersect`], the effective
//!   window.
//! - Configurable: nothing. The accepted spelling is RFC 3339 with a `Z`
//!   offset, which is what every example in consent.md writes.

use std::fmt;

use serde::{Deserialize, Serialize};

/// An instant, stored as seconds since the Unix epoch and written back as
/// `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Only the `Z` offset is accepted. A numeric offset would have to be
/// normalized before hashing or two spellings of one instant would produce
/// two `request_hash`es, and refusing the spelling is cheaper than
/// normalizing it — there is no caller that wants to write one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Timestamp(i64);

impl Timestamp {
    pub fn from_unix_secs(secs: i64) -> Timestamp {
        Timestamp(secs)
    }

    pub fn unix_secs(self) -> i64 {
        self.0
    }

    /// Is this instant strictly after `now`? §7 step 4's question, with
    /// "now" supplied rather than read.
    pub fn is_after(self, now: Timestamp) -> bool {
        self.0 > now.0
    }

    pub fn parse(text: &str) -> Result<Timestamp, BadTimestamp> {
        let bad = || BadTimestamp {
            text: text.to_string(),
        };
        // YYYY-MM-DDTHH:MM:SSZ, fixed width, which is the only form written.
        let b = text.as_bytes();
        if b.len() != 20
            || b[4] != b'-'
            || b[7] != b'-'
            || b[10] != b'T'
            || b[13] != b':'
            || b[16] != b':'
            || b[19] != b'Z'
        {
            return Err(bad());
        }
        let num = |from: usize, to: usize| -> Result<i64, BadTimestamp> {
            text.get(from..to)
                .ok_or_else(bad)?
                .parse::<i64>()
                .map_err(|_| bad())
        };
        let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
        let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return Err(bad());
        }
        // A leap second (`:60`) is a real RFC 3339 spelling and is refused:
        // nothing in this system writes one, and accepting it would mean
        // two instants comparing equal after normalization.
        if hour > 23 || minute > 59 || second > 59 {
            return Err(bad());
        }
        Ok(Timestamp(
            days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("'{text}' is not an instant; write it as YYYY-MM-DDTHH:MM:SSZ")]
pub struct BadTimestamp {
    pub text: String,
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self.0.div_euclid(86_400);
        let rest = self.0.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            rest / 3_600,
            (rest % 3_600) / 60,
            rest % 60
        )
    }
}

impl From<Timestamp> for String {
    fn from(t: Timestamp) -> String {
        t.to_string()
    }
}

impl TryFrom<String> for Timestamp {
    type Error = BadTimestamp;
    fn try_from(s: String) -> Result<Timestamp, BadTimestamp> {
        Timestamp::parse(&s)
    }
}

/// A validity window: consent.md §6's `[valid_from, valid_until]`, and the
/// thing an approval's `not_after` is intersected with.
///
/// Both ends optional, because "access to this, from now, forever" is a
/// legitimate ask and absent means unbounded on that side. An absent field
/// is omitted from the hashed object — never `null` — which is the one
/// place §9's omitted-never-blanked rule actually bites.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Window {
    pub from: Option<Timestamp>,
    pub until: Option<Timestamp>,
}

impl Window {
    /// The effective window: the tighter of each end.
    ///
    /// An operator can always grant less than was asked and never more,
    /// which is what taking the later `from` and the earlier `until` means.
    pub fn intersect(self, other: Window) -> Window {
        Window {
            from: match (self.from, other.from) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            },
            until: match (self.until, other.until) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
        }
    }

    /// Is there any instant in this window? §7 step 4's second half.
    pub fn is_non_empty(self) -> bool {
        match (self.from, self.until) {
            (Some(from), Some(until)) => from <= until,
            _ => true,
        }
    }

    /// Does this window contain `now`? What a held grant is checked against
    /// on the call after it was granted.
    pub fn contains(self, now: Timestamp) -> bool {
        self.from.is_none_or(|from| from <= now) && self.until.is_none_or(|until| now <= until)
    }
}

// depth: the civil-date arithmetic, Howard Hinnant's algorithms

/// Days from 1970-01-01 for a civil date. `days_from_civil` from Hinnant's
/// `chrono`-compatible algorithms, which is the standard reference
/// implementation and correct for the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse, for `Display`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_examples_from_consent_md_round_trip() {
        for text in [
            "2026-09-11T20:14:00Z",
            "2026-09-12T00:00:00Z",
            "2026-09-19T00:00:00Z",
            "1970-01-01T00:00:00Z",
        ] {
            let t = Timestamp::parse(text).unwrap();
            assert_eq!(t.to_string(), text, "round trip");
        }
        assert_eq!(
            Timestamp::parse("1970-01-01T00:00:00Z")
                .unwrap()
                .unix_secs(),
            0
        );
        assert_eq!(
            Timestamp::parse("2026-09-12T00:00:00Z")
                .unwrap()
                .unix_secs(),
            1_789_171_200
        );
    }

    /// A leap year, and a date after one, because off-by-one in the civil
    /// arithmetic is the failure nobody notices until February.
    #[test]
    fn leap_days_are_right() {
        assert_eq!(
            Timestamp::parse("2024-02-29T12:00:00Z")
                .unwrap()
                .to_string(),
            "2024-02-29T12:00:00Z"
        );
        assert_eq!(
            Timestamp::parse("2024-03-01T00:00:00Z")
                .unwrap()
                .to_string(),
            "2024-03-01T00:00:00Z"
        );
        assert_eq!(
            Timestamp::parse("2100-03-01T00:00:00Z")
                .unwrap()
                .to_string(),
            "2100-03-01T00:00:00Z"
        );
    }

    #[test]
    fn an_offset_other_than_z_is_refused() {
        for text in [
            "2026-09-11T20:14:00+01:00",
            "2026-09-11 20:14:00Z",
            "2026-09-11T20:14:00",
            "2026-13-01T00:00:00Z",
            "2026-09-11T24:00:00Z",
            "2026-09-11T23:59:60Z",
        ] {
            assert!(Timestamp::parse(text).is_err(), "{text}");
        }
    }

    /// The effective window takes the tighter end on both sides: an
    /// operator grants less than was asked, never more.
    #[test]
    fn intersection_never_widens() {
        let t = |s: &str| Timestamp::parse(s).unwrap();
        let asked = Window {
            from: Some(t("2026-09-12T00:00:00Z")),
            until: Some(t("2026-09-19T00:00:00Z")),
        };
        let approved = Window {
            from: None,
            until: Some(t("2026-09-13T00:00:00Z")),
        };
        let effective = asked.intersect(approved);
        assert_eq!(effective.from, asked.from, "the later start wins");
        assert_eq!(effective.until, approved.until, "the earlier end wins");
        assert!(effective.is_non_empty());

        // An approval ending before the ask begins grants nothing, and
        // says so rather than granting the ask.
        let stale = asked.intersect(Window {
            from: None,
            until: Some(t("2026-09-11T00:00:00Z")),
        });
        assert!(!stale.is_non_empty());
    }

    #[test]
    fn an_unbounded_window_contains_everything() {
        let now = Timestamp::from_unix_secs(1_757_707_440);
        assert!(Window::default().is_non_empty());
        assert!(Window::default().contains(now));
        assert!(Timestamp::from_unix_secs(now.unix_secs() + 1).is_after(now));
        assert!(!now.is_after(now), "strictly after");
    }
}
