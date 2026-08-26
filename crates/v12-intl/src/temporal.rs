//! ISO date/time records for the Temporal support surface, backed by
//! `temporal_rs`.
//!
//! These are v12-owned thin structs rather than re-exports of
//! `temporal_rs::iso::{IsoDate, IsoTime, IsoDateTime}`: those records are
//! documented as internal calculation types and their validating constructors
//! are not public. All real validation and arithmetic is delegated to
//! `temporal_rs`'s public ISO-calendar operations (`PlainDate`,
//! `PlainDateTime`, `Duration`) so v12 never forks Temporal semantics.
//!
//! Calendar systems beyond ISO and timezone support are future waves
//! (see lib.rs coverage status).

use crate::error::IntlError;
use temporal_rs::options::Overflow;
use temporal_rs::{Calendar, Duration, PlainDate, PlainDateTime};

/// An ISO 8601 calendar date: year, month (1–12), day (1–31, month-valid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct IsoDate {
    year: i32,
    month: u8,
    day: u8,
}

impl IsoDate {
    /// Creates a validated date. Rejects invalid month/day combinations
    /// (including leap-year violations) and dates outside the Temporal
    /// representable window.
    pub fn new(year: i32, month: u8, day: u8) -> Result<Self, IntlError> {
        // Delegate to temporal_rs: RegulateISODate(Reject) + within-limits check.
        PlainDate::try_new(year, month, day, Calendar::default()).map_err(map_temporal)?;
        Ok(Self { year, month, day })
    }

    pub const fn year(&self) -> i32 {
        self.year
    }

    pub const fn month(&self) -> u8 {
        self.month
    }

    pub const fn day(&self) -> u8 {
        self.day
    }

    /// Returns the date shifted by `days` (negative shifts backwards),
    /// balanced across month/year boundaries. Errors if the result leaves the
    /// representable range.
    pub fn add_days(&self, days: i64) -> Result<Self, IntlError> {
        self.shift(days, false)
    }

    /// Returns the date shifted back by `days`. Errors if the result leaves
    /// the representable range.
    pub fn sub_days(&self, days: i64) -> Result<Self, IntlError> {
        self.shift(days, true)
    }

    fn shift(&self, days: i64, backward: bool) -> Result<Self, IntlError> {
        // Positional slots: years, months, weeks, DAYS, hours, minutes,
        // seconds, milliseconds, microseconds, nanoseconds — only days moves.
        let duration = Duration::new(0, 0, 0, days, 0, 0, 0, 0, 0, 0).map_err(map_temporal)?;
        let plain = self.to_plain_date()?;
        let shifted = if backward {
            plain.subtract(&duration, Some(Overflow::Reject))
        } else {
            plain.add(&duration, Some(Overflow::Reject))
        }
        .map_err(map_temporal)?;
        Self::new(shifted.year(), shifted.month(), shifted.day())
    }

    /// Formats as an ISO 8601 date string per Temporal's rules
    /// (`"2026-02-01"`; expanded years use a mandatory sign, e.g.
    /// `"+010000-01-01"`).
    pub fn to_iso_string(&self) -> Result<String, IntlError> {
        Ok(self.to_plain_date()?.to_string())
    }

    fn to_plain_date(self) -> Result<PlainDate, IntlError> {
        PlainDate::try_new(self.year, self.month, self.day, Calendar::default())
            .map_err(map_temporal)
    }
}

/// A wall-clock time with sub-second precision down to nanoseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct IsoTime {
    hour: u8,
    minute: u8,
    second: u8,
    millisecond: u16,
    microsecond: u16,
    nanosecond: u16,
}

impl IsoTime {
    /// Midnight (`00:00:00`), the default time for date-only values.
    pub const MIDNIGHT: IsoTime = IsoTime {
        hour: 0,
        minute: 0,
        second: 0,
        millisecond: 0,
        microsecond: 0,
        nanosecond: 0,
    };

    /// Creates a validated time; rejects out-of-range components
    /// (ES `IsValidTime`).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        hour: u8,
        minute: u8,
        second: u8,
        millisecond: u16,
        microsecond: u16,
        nanosecond: u16,
    ) -> Result<Self, IntlError> {
        // ISO 8601 time-component bounds (Temporal spec grammar).
        const MAX_HOUR: u8 = 23;
        const MAX_MINUTE_OR_SECOND: u8 = 59;
        const MAX_SUBSECOND_UNIT: u16 = 999;

        let valid = hour <= MAX_HOUR
            && minute <= MAX_MINUTE_OR_SECOND
            && second <= MAX_MINUTE_OR_SECOND
            && millisecond <= MAX_SUBSECOND_UNIT
            && microsecond <= MAX_SUBSECOND_UNIT
            && nanosecond <= MAX_SUBSECOND_UNIT;
        if !valid {
            return Err(IntlError::Range("invalid time components".to_string()));
        }
        Ok(Self {
            hour,
            minute,
            second,
            millisecond,
            microsecond,
            nanosecond,
        })
    }

    pub const fn hour(&self) -> u8 {
        self.hour
    }

    pub const fn minute(&self) -> u8 {
        self.minute
    }

    pub const fn second(&self) -> u8 {
        self.second
    }

    pub const fn millisecond(&self) -> u16 {
        self.millisecond
    }

    pub const fn microsecond(&self) -> u16 {
        self.microsecond
    }

    pub const fn nanosecond(&self) -> u16 {
        self.nanosecond
    }
}

/// An ISO date plus wall-clock time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct IsoDateTime {
    date: IsoDate,
    time: IsoTime,
}

impl IsoDateTime {
    /// Creates a validated date-time; rejects invalid components and
    /// date-times outside the Temporal representable window.
    pub fn new(date: IsoDate, time: IsoTime) -> Result<Self, IntlError> {
        PlainDateTime::try_new(
            date.year(),
            date.month(),
            date.day(),
            time.hour(),
            time.minute(),
            time.second(),
            time.millisecond(),
            time.microsecond(),
            time.nanosecond(),
            Calendar::default(),
        )
        .map_err(map_temporal)?;
        Ok(Self { date, time })
    }

    pub const fn date(&self) -> &IsoDate {
        &self.date
    }

    pub const fn time(&self) -> &IsoTime {
        &self.time
    }

    /// Formats per Temporal `PlainDateTime` toString rules, e.g.
    /// `"2026-02-01T13:45:30"`.
    pub fn to_iso_string(&self) -> Result<String, IntlError> {
        let t = &self.time;
        PlainDateTime::try_new(
            self.date.year(),
            self.date.month(),
            self.date.day(),
            t.hour(),
            t.minute(),
            t.second(),
            t.millisecond(),
            t.microsecond(),
            t.nanosecond(),
            Calendar::default(),
        )
        .map(|dt| dt.to_string())
        .map_err(map_temporal)
    }
}

fn map_temporal(error: temporal_rs::TemporalError) -> IntlError {
    IntlError::Range(error.into_message().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_boundary_add() {
        let d = IsoDate::new(2026, 1, 31).unwrap();
        assert_eq!(d.add_days(1).unwrap(), IsoDate::new(2026, 2, 1).unwrap());
    }

    #[test]
    fn year_boundary_and_backwards() {
        let d = IsoDate::new(2026, 12, 31).unwrap();
        assert_eq!(d.add_days(1).unwrap(), IsoDate::new(2027, 1, 1).unwrap());
        assert_eq!(
            d.sub_days(365).unwrap(),
            IsoDate::new(2025, 12, 31).unwrap()
        );
    }

    #[test]
    fn leap_years() {
        // 2024 is a leap year: Feb 29 exists, and adding one day lands Mar 1...
        let d = IsoDate::new(2024, 2, 28).unwrap();
        assert_eq!(d.add_days(1).unwrap(), IsoDate::new(2024, 2, 29).unwrap());
        assert_eq!(d.add_days(2).unwrap(), IsoDate::new(2024, 3, 1).unwrap());
        // ...while 2026 is not.
        let d = IsoDate::new(2026, 2, 28).unwrap();
        assert_eq!(d.add_days(1).unwrap(), IsoDate::new(2026, 3, 1).unwrap());
        // Century rule: 2000 leap, 1900 not.
        assert!(IsoDate::new(2000, 2, 29).is_ok());
        assert!(IsoDate::new(1900, 2, 29).is_err());
    }

    #[test]
    fn invalid_dates_rejected() {
        assert!(IsoDate::new(2026, 13, 1).is_err());
        assert!(IsoDate::new(2026, 0, 10).is_err());
        assert!(IsoDate::new(2026, 4, 31).is_err());
        assert!(IsoDate::new(2026, 2, 29).is_err());
    }

    #[test]
    fn iso_strings() {
        assert_eq!(
            IsoDate::new(2026, 2, 1).unwrap().to_iso_string().unwrap(),
            "2026-02-01"
        );
        let dt = IsoDateTime::new(
            IsoDate::new(2026, 2, 1).unwrap(),
            IsoTime::new(13, 45, 30, 123, 0, 0).unwrap(),
        )
        .unwrap();
        assert_eq!(dt.to_iso_string().unwrap(), "2026-02-01T13:45:30.123");
    }

    #[test]
    fn invalid_times_rejected() {
        assert!(IsoTime::new(24, 0, 0, 0, 0, 0).is_err());
        assert!(IsoTime::new(0, 60, 0, 0, 0, 0).is_err());
        assert!(IsoTime::new(0, 0, 60, 0, 0, 0).is_err());
        assert!(IsoTime::new(0, 0, 0, 1000, 0, 0).is_err());
        assert!(IsoTime::MIDNIGHT == IsoTime::new(0, 0, 0, 0, 0, 0).unwrap());
    }

    #[test]
    fn out_of_range_shift_errors() {
        // Bounds follow temporal_rs's within-limits check exactly (v12 owns no
        // calendar math): dates are validated at noon against an instant
        // window with one day of slack.
        //
        // Upper end: +1 day at noon overshoots the +24h margin -> rejected.
        let max = IsoDate::new(275_760, 9, 13).unwrap();
        assert!(max.add_days(1).is_err());
        // Lower end: -1 day stays inside the margin (accepted), -2 does not.
        // This asymmetry is upstream temporal_rs 0.2.x behavior, not ours.
        let min = IsoDate::new(-271_821, 4, 20).unwrap();
        assert!(min.sub_days(1).is_ok());
        assert!(min.sub_days(2).is_err());
    }
}
