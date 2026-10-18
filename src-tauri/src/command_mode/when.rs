//! When a reminder is for (PARLA-86, spec §1 and §2): the model gives the
//! local day and time it heard, and Parla decides what Traccia gets, here,
//! deterministically. A day and a time become a UTC `remindAt` by Windows'
//! own time-zone rules, so DST is right and "at 3" means 3 wherever Mattia
//! is; a day alone gets `DEFAULT_REMINDER_TIME` if that is still ahead, else
//! no Reminder; nothing at all is an Anytime To-do. A moment already past is
//! sent all the same: Traccia refuses it, the one source of truth.

use std::fmt;

/// A day said with no time (Q2).
pub const DEFAULT_REMINDER_TIME: Time = Time { hour: 9, minute: 0 };

/// A calendar day, `YYYY-MM-DD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    pub year: i32,
    pub month: u8,
    pub day: u8,
}

/// A wall-clock time, `HH:MM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Time {
    pub hour: u8,
    pub minute: u8,
}

/// The local date and time at key-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalNow {
    pub date: Date,
    pub time: Time,
}

/// What the To-do is created with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    /// A Reminder at this local day and time. `utc` is the moment as
    /// `remindAt`; None off Windows, where the day and time go as said.
    At {
        day: Date,
        time: Time,
        utc: Option<String>,
    },
    /// A Planned day with no Reminder.
    Day(Date),
    /// No day, no Reminder.
    Anytime,
}

/// The local time zone.
pub trait LocalClock {
    /// The local wall-clock time as a UTC `remindAt` (`…Z`). A time said
    /// twice when the clocks go back is the first; one skipped when they go
    /// forward is the hour after. None where Parla can't tell.
    fn to_utc(&self, day: Date, time: Time) -> Option<String>;
}

/// The spec's §2 table.
pub fn resolve(
    day: Option<Date>,
    time: Option<Time>,
    now: LocalNow,
    clock: &dyn LocalClock,
) -> When {
    let at = |day, time| When::At {
        day,
        time,
        utc: clock.to_utc(day, time),
    };
    match (day, time) {
        (Some(day), Some(time)) => at(day, time),
        (Some(day), None) if (day, DEFAULT_REMINDER_TIME) > (now.date, now.time) => {
            at(day, DEFAULT_REMINDER_TIME)
        }
        (Some(day), None) => When::Day(day),
        // The prompt asks for the day; should it be missing, the same rule.
        (None, Some(time)) if time > now.time => at(now.date, time),
        (None, Some(time)) => at(now.date.next(), time),
        (None, None) => When::Anytime,
    }
}

/// The machine's clock and time zone.
pub struct SystemClock;

impl SystemClock {
    #[cfg(windows)]
    pub fn now(&self) -> LocalNow {
        let mut now = Default::default();
        // SAFETY: GetLocalTime only fills the plain-data SYSTEMTIME.
        unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut now) };
        windows_zone::read(&now)
    }

    /// Off Windows, where Parla does not run: UTC.
    #[cfg(not(windows))]
    pub fn now(&self) -> LocalNow {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let (date, time) = from_minutes(seconds / 60);
        LocalNow { date, time }
    }
}

impl LocalClock for SystemClock {
    #[cfg(windows)]
    fn to_utc(&self, day: Date, time: Time) -> Option<String> {
        // Null: the zone Windows is set to, with its DST rules.
        windows_zone::to_utc(std::ptr::null(), day, time)
    }

    /// Off Windows: no conversion, the day and time go as said.
    #[cfg(not(windows))]
    fn to_utc(&self, _day: Date, _time: Time) -> Option<String> {
        None
    }
}

#[cfg(windows)]
mod windows_zone {
    use super::{from_minutes, Date, LocalNow, Time};
    use windows_sys::Win32::{
        Foundation::SYSTEMTIME,
        System::Time::{
            SystemTimeToTzSpecificLocalTime, TzSpecificLocalTimeToSystemTime, TIME_ZONE_INFORMATION,
        },
    };

    pub(super) fn read(time: &SYSTEMTIME) -> LocalNow {
        LocalNow {
            date: Date {
                year: time.wYear.into(),
                month: time.wMonth as u8,
                day: time.wDay as u8,
            },
            time: Time {
                hour: time.wHour as u8,
                minute: time.wMinute as u8,
            },
        }
    }

    fn write(at: LocalNow) -> SYSTEMTIME {
        SYSTEMTIME {
            wYear: at.date.year as u16,
            wMonth: at.date.month.into(),
            wDay: at.date.day.into(),
            wHour: at.time.hour.into(),
            wMinute: at.time.minute.into(),
            ..Default::default()
        }
    }

    /// `zone` null means the machine's.
    pub(super) fn to_utc(
        zone: *const TIME_ZONE_INFORMATION,
        day: Date,
        time: Time,
    ) -> Option<String> {
        let said = LocalNow { date: day, time };
        let mut moment = convert(zone, said)?;
        let reads_as_said = |moment| back(zone, moment) == Some(said);
        if !reads_as_said(moment) {
            // Skipped as the clocks go forward: the hour after.
            let (date, time) = from_minutes(minutes(said) + 60);
            moment = convert(zone, LocalNow { date, time })?;
        } else if reads_as_said(moment - 60) {
            // Said twice as the clocks go back: the first.
            moment -= 60;
        }
        let (day, time) = from_minutes(moment);
        Some(format!("{day}T{time}:00Z"))
    }

    /// Minutes since 1970-01-01 00:00.
    fn minutes(at: LocalNow) -> i64 {
        at.date.days() * 1440 + i64::from(at.time.hour) * 60 + i64::from(at.time.minute)
    }

    /// Local to UTC minutes, as Windows reads it.
    fn convert(zone: *const TIME_ZONE_INFORMATION, local: LocalNow) -> Option<i64> {
        let mut utc = SYSTEMTIME::default();
        // SAFETY: plain data in and out; a null zone is documented.
        let ok = unsafe { TzSpecificLocalTimeToSystemTime(zone, &write(local), &mut utc) };
        (ok != 0).then(|| minutes(read(&utc)))
    }

    /// UTC minutes back to local.
    fn back(zone: *const TIME_ZONE_INFORMATION, moment: i64) -> Option<LocalNow> {
        let (date, time) = from_minutes(moment);
        let mut local = SYSTEMTIME::default();
        // SAFETY: as above.
        let ok = unsafe {
            SystemTimeToTzSpecificLocalTime(zone, &write(LocalNow { date, time }), &mut local)
        };
        (ok != 0).then(|| read(&local))
    }
}

fn from_minutes(minutes: i64) -> (Date, Time) {
    let of_day = minutes.rem_euclid(1440);
    (
        Date::from_days(minutes.div_euclid(1440)),
        Time {
            hour: (of_day / 60) as u8,
            minute: (of_day % 60) as u8,
        },
    )
}

impl Date {
    /// Strictly `YYYY-MM-DD`, and a day the calendar has.
    pub fn parse(text: &str) -> Option<Self> {
        let [year, month, day] = digits(text, &[4, 2, 2], b'-')?;
        let date = Self {
            year: year as i32,
            month: month as u8,
            day: day as u8,
        };
        (date.month >= 1
            && date.month <= 12
            && date.day >= 1
            && date.day <= days_in_month(date.year, date.month))
        .then_some(date)
    }

    pub fn next(self) -> Self {
        Self::from_days(self.days() + 1)
    }

    pub fn weekday(self) -> &'static str {
        const WEEKDAYS: [&str; 7] = [
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
            "Sunday",
        ];
        // 1970-01-01 was a Thursday.
        WEEKDAYS[(self.days() + 3).rem_euclid(7) as usize]
    }

    /// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
    pub(crate) fn days(self) -> i64 {
        let month = i64::from(self.month);
        let year = i64::from(self.year) - i64::from(month <= 2);
        let era = year.div_euclid(400);
        let year_of_era = year - era * 400;
        let day_of_year =
            (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(self.day) - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        era * 146_097 + day_of_era - 719_468
    }

    /// The inverse of `days` (`civil_from_days`).
    pub(crate) fn from_days(days: i64) -> Self {
        let days = days + 719_468;
        let era = days.div_euclid(146_097);
        let day_of_era = days - era * 146_097;
        let year_of_era =
            (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let shifted_month = (5 * day_of_year + 2) / 153;
        let month = if shifted_month < 10 {
            shifted_month + 3
        } else {
            shifted_month - 9
        };
        Self {
            year: (year_of_era + era * 400 + i64::from(month <= 2)) as i32,
            month: month as u8,
            day: (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u8,
        }
    }
}

fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Fixed-width ASCII digit groups joined by `separator`.
fn digits<const N: usize>(text: &str, widths: &[usize; N], separator: u8) -> Option<[u32; N]> {
    let bytes = text.as_bytes();
    if bytes.len() != widths.iter().sum::<usize>() + N - 1 {
        return None;
    }
    let mut values = [0; N];
    let mut at = 0;
    for (value, width) in values.iter_mut().zip(widths) {
        if at > 0 {
            (bytes[at] == separator).then_some(())?;
            at += 1;
        }
        let group = &bytes[at..at + width];
        group.iter().all(u8::is_ascii_digit).then_some(())?;
        *value = group
            .iter()
            .fold(0, |sum, digit| sum * 10 + u32::from(digit - b'0'));
        at += width;
    }
    Some(values)
}

impl Time {
    /// Strictly `HH:MM`, 00:00 to 23:59.
    pub fn parse(text: &str) -> Option<Self> {
        let [hour, minute] = digits(text, &[2, 2], b':')?;
        (hour < 24 && minute < 60).then_some(Self {
            hour: hour as u8,
            minute: minute as u8,
        })
    }
}

impl LocalNow {
    /// The request's context line: `Saturday 2026-10-17 21:14`.
    pub fn line(&self) -> String {
        format!("{} {} {}", self.date.weekday(), self.date, self.time)
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

/// Shared by the command-mode tests: Saturday evening in Rome in October.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{Date, LocalClock, LocalNow, Time};

    pub(crate) fn date(text: &str) -> Date {
        Date::parse(text).unwrap()
    }

    pub(crate) fn time(text: &str) -> Time {
        Time::parse(text).unwrap()
    }

    pub(crate) fn at(day: &str, hour: &str) -> LocalNow {
        LocalNow {
            date: date(day),
            time: time(hour),
        }
    }

    pub(crate) fn saturday_evening() -> LocalNow {
        at("2026-10-17", "21:14")
    }

    /// Rome before the clocks go back: UTC+2.
    pub(crate) struct RomeInOctober;
    impl LocalClock for RomeInOctober {
        fn to_utc(&self, day: Date, time: Time) -> Option<String> {
            let hour = time.hour.checked_sub(2)?;
            Some(format!("{day}T{hour:02}:{:02}:00Z", time.minute))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::test_support::{at, date, time, RomeInOctober};

    /// A zone Parla can't read.
    struct NoZone;
    impl LocalClock for NoZone {
        fn to_utc(&self, _day: Date, _time: Time) -> Option<String> {
            None
        }
    }

    fn resolved(day: Option<&str>, hour: Option<&str>, now: LocalNow) -> When {
        resolve(day.map(date), hour.map(time), now, &RomeInOctober)
    }

    #[test]
    fn a_day_and_a_time_become_a_utc_reminder() {
        let saturday_evening = at("2026-10-17", "21:14");
        assert_eq!(
            resolved(Some("2026-10-18"), Some("15:00"), saturday_evening),
            When::At {
                day: date("2026-10-18"),
                time: time("15:00"),
                utc: Some("2026-10-18T13:00:00Z".into()),
            }
        );
        // Already past: sent as said, for Traccia to refuse (Q4).
        assert_eq!(
            resolved(Some("2026-10-17"), Some("10:00"), saturday_evening),
            When::At {
                day: date("2026-10-17"),
                time: time("10:00"),
                utc: Some("2026-10-17T08:00:00Z".into()),
            }
        );
    }

    #[test]
    fn a_day_alone_gets_nine_oclock_while_that_is_still_ahead() {
        let saturday_evening = at("2026-10-17", "21:14");
        assert_eq!(
            resolved(Some("2026-10-19"), None, saturday_evening),
            When::At {
                day: date("2026-10-19"),
                time: DEFAULT_REMINDER_TIME,
                utc: Some("2026-10-19T07:00:00Z".into()),
            }
        );
        // Today, after 09:00: the day only, no Reminder (Q2).
        assert_eq!(
            resolved(Some("2026-10-17"), None, saturday_evening),
            When::Day(date("2026-10-17"))
        );
        assert_eq!(
            resolved(Some("2026-10-17"), None, at("2026-10-17", "09:00")),
            When::Day(date("2026-10-17"))
        );
        assert_eq!(
            resolved(Some("2026-10-17"), None, at("2026-10-17", "08:59")),
            When::At {
                day: date("2026-10-17"),
                time: DEFAULT_REMINDER_TIME,
                utc: Some("2026-10-17T07:00:00Z".into()),
            }
        );
    }

    #[test]
    fn nothing_said_is_an_anytime_to_do() {
        assert_eq!(
            resolved(None, None, at("2026-10-17", "21:14")),
            When::Anytime
        );
    }

    #[test]
    fn a_time_alone_is_today_or_tomorrow_once_it_has_passed() {
        let now = at("2026-10-17", "21:14");
        assert_eq!(
            resolved(None, Some("22:00"), now),
            When::At {
                day: date("2026-10-17"),
                time: time("22:00"),
                utc: Some("2026-10-17T20:00:00Z".into()),
            }
        );
        for passed in ["08:00", "21:14"] {
            assert!(
                matches!(
                    resolved(None, Some(passed), now),
                    When::At { day, .. } if day == date("2026-10-18")
                ),
                "{passed}"
            );
        }
        // Across a month and a year.
        assert!(matches!(
            resolved(None, Some("08:00"), at("2026-12-31", "21:14")),
            When::At { day, .. } if day == date("2027-01-01")
        ));
    }

    #[test]
    fn a_clock_that_cannot_convert_sends_the_day_and_time_as_said() {
        assert_eq!(
            resolve(
                Some(date("2026-10-18")),
                Some(time("15:00")),
                at("2026-10-17", "21:14"),
                &NoZone
            ),
            When::At {
                day: date("2026-10-18"),
                time: time("15:00"),
                utc: None,
            }
        );
    }

    #[test]
    fn days_and_times_parse_strictly() {
        assert_eq!(
            Date::parse("2026-10-17"),
            Some(Date {
                year: 2026,
                month: 10,
                day: 17
            })
        );
        assert!(Date::parse("2028-02-29").is_some(), "a leap day");
        for bad in [
            "2026-02-29",
            "2100-02-29",
            "2026-13-01",
            "2026-00-10",
            "2026-04-31",
            "2026-10-00",
            "2026-1-01",
            "26-10-17",
            "2026/10/17",
            "2026-10-17T10:00",
            " 2026-10-17",
            "",
        ] {
            assert_eq!(Date::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Time::parse("09:05"), Some(Time { hour: 9, minute: 5 }));
        assert_eq!(
            Time::parse("23:59").map(|t| t.to_string()).as_deref(),
            Some("23:59")
        );
        for bad in [
            "24:00", "9:00", "09:60", "09:5", "0900", "09:00:00", "", "ab:cd",
        ] {
            assert_eq!(Time::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_next_day_rolls_over_months_years_and_leap_days() {
        for (day, next) in [
            ("2026-10-17", "2026-10-18"),
            ("2026-10-31", "2026-11-01"),
            ("2026-12-31", "2027-01-01"),
            ("2026-02-28", "2026-03-01"),
            ("2028-02-28", "2028-02-29"),
        ] {
            assert_eq!(date(day).next(), date(next), "{day}");
        }
    }

    #[test]
    fn the_context_line_names_the_weekday() {
        assert_eq!(
            at("2026-10-17", "21:14").line(),
            "Saturday 2026-10-17 21:14"
        );
        assert_eq!(at("2026-10-19", "10:00").line(), "Monday 2026-10-19 10:00");
        assert_eq!(date("2000-01-01").weekday(), "Saturday");
        assert_eq!(date("2027-03-28").weekday(), "Sunday");
    }

    /// Europe/Rome's rules, given to the real conversion so the test holds
    /// whatever zone the machine is in.
    #[cfg(windows)]
    fn rome() -> windows_sys::Win32::System::Time::TIME_ZONE_INFORMATION {
        use windows_sys::Win32::{Foundation::SYSTEMTIME, System::Time::TIME_ZONE_INFORMATION};
        // The last Sunday of the month, at `hour`.
        let last_sunday = |month, hour| SYSTEMTIME {
            wMonth: month,
            wDayOfWeek: 0,
            wDay: 5,
            wHour: hour,
            ..Default::default()
        };
        TIME_ZONE_INFORMATION {
            Bias: -60,
            StandardDate: last_sunday(10, 3),
            DaylightDate: last_sunday(3, 2),
            DaylightBias: -60,
            ..Default::default()
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_converts_rome_time_to_utc_across_the_dst_edges() {
        let zone = rome();
        let utc = |day: &str, hour: &str| windows_zone::to_utc(&zone, date(day), time(hour));
        assert_eq!(
            utc("2026-10-18", "15:00").as_deref(),
            Some("2026-10-18T13:00:00Z")
        );
        assert_eq!(
            utc("2026-11-03", "15:00").as_deref(),
            Some("2026-11-03T14:00:00Z")
        );
        // 02:30 happens twice on 2026-10-25: the first, still +02:00.
        assert_eq!(
            utc("2026-10-25", "02:30").as_deref(),
            Some("2026-10-25T00:30:00Z")
        );
        assert_eq!(
            utc("2026-10-25", "03:30").as_deref(),
            Some("2026-10-25T02:30:00Z")
        );
        // 02:30 never happens on 2027-03-28: the hour after, 03:30 +02:00.
        assert_eq!(
            utc("2027-03-28", "02:30").as_deref(),
            Some("2027-03-28T01:30:00Z")
        );
        assert_eq!(
            utc("2027-03-28", "01:30").as_deref(),
            Some("2027-03-28T00:30:00Z")
        );
        // Midnight crosses back a day.
        assert_eq!(
            utc("2027-01-01", "00:30").as_deref(),
            Some("2026-12-31T23:30:00Z")
        );
    }

    #[test]
    fn the_system_clock_reads_a_plausible_now() {
        let now = SystemClock.now();
        assert!(now.date.year >= 2026, "{now:?}");
        assert!(Date::parse(&now.date.to_string()).is_some(), "{now:?}");
    }
}
