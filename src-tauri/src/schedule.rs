//! Daily recurring queue window: the queue runs only inside a
//! start/stop time window on selected weekdays, and only while ZDM
//! is running (there is no autostart — that is a separate feature).
//!
//! Pure logic, no I/O, so it is unit-testable with an injected clock.
//!
//! Precedence (also documented in README.md):
//! - A pending one-shot `schedule_queue` and a manual Start/Stop
//!   hold the daily window until the next window boundary; after
//!   that the daily window takes over again.
//! - Inside the window the queue is started; outside it the queue
//!   is stopped and running downloads are paused (never cancelled,
//!   so they resume from where they left off).

use chrono::{DateTime, Datelike, Local, NaiveTime, TimeZone, Timelike, Weekday};
use serde::{Deserialize, Serialize};

pub const DEFAULT_START: &str = "23:00";
pub const DEFAULT_STOP: &str = "06:00";

fn default_start() -> String {
    DEFAULT_START.to_string()
}

fn default_stop() -> String {
    DEFAULT_STOP.to_string()
}

fn default_days() -> [bool; 7] {
    [true; 7]
}

/// Weekday index into `days`: 0 = Monday .. 6 = Sunday
/// (`Weekday::num_days_from_monday`).
pub fn weekday_index(day: Weekday) -> usize {
    day.num_days_from_monday() as usize
}

fn short_weekday(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailySchedule {
    #[serde(default)]
    pub enabled: bool,
    /// "HH:MM", local time.
    #[serde(default = "default_start")]
    pub start: String,
    /// "HH:MM", local time.
    #[serde(default = "default_stop")]
    pub stop: String,
    /// Selected days, Monday .. Sunday.
    #[serde(default = "default_days")]
    pub days: [bool; 7],
}

impl Default for DailySchedule {
    fn default() -> Self {
        Self {
            enabled: false,
            start: DEFAULT_START.to_string(),
            stop: DEFAULT_STOP.to_string(),
            days: [true; 7],
        }
    }
}

/// What the next schedule transition does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextKind {
    Start,
    Stop,
}

impl DailySchedule {
    /// Parse "HH:MM" strictly; anything else is invalid.
    pub fn parse_time(s: &str) -> Option<NaiveTime> {
        NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()
    }

    /// Start/stop as times, falling back to the defaults when a
    /// stored value is malformed (a hand-edited settings.json must
    /// never break the scheduler).
    pub fn window(&self) -> (NaiveTime, NaiveTime) {
        let start = Self::parse_time(&self.start)
            .unwrap_or_else(|| NaiveTime::from_hms_opt(23, 0, 0).unwrap());
        let stop = Self::parse_time(&self.stop)
            .unwrap_or_else(|| NaiveTime::from_hms_opt(6, 0, 0).unwrap());
        (start, stop)
    }

    /// Is `now` inside the window? Handles windows that cross
    /// midnight (stop < start, e.g. 23:00–06:00). The after-midnight
    /// part belongs to the day the window started on, so a Monday
    /// 23:00–06:00 window still covers Tuesday 01:00.
    pub fn contains(&self, now: DateTime<Local>) -> bool {
        if !self.enabled {
            return false;
        }
        let (start, stop) = self.window();
        let t = now.time();
        let today = weekday_index(now.weekday());
        if start == stop {
            // Degenerate window: the whole day, on selected days.
            return self.days[today];
        }
        if start < stop {
            self.days[today] && t >= start && t < stop
        } else if t >= start {
            self.days[today]
        } else if t < stop {
            // After midnight: yesterday's window is still running.
            let yesterday = now - chrono::Duration::days(1);
            self.days[weekday_index(yesterday.weekday())]
        } else {
            false
        }
    }

    /// The next transition after `now`, if the schedule is enabled
    /// and at least one day is selected. Scans today + the next 7
    /// days so a window on a single weekday is found.
    pub fn next_event(&self, now: DateTime<Local>) -> Option<(NextKind, DateTime<Local>)> {
        if !self.enabled || self.days.iter().all(|d| !*d) {
            return None;
        }
        let (start, stop) = self.window();
        let mut best: Option<(NextKind, DateTime<Local>)> = None;
        // Start at yesterday: a window that began yesterday and ends
        // today (e.g. Monday 23:00–06:00, now Tuesday 01:00) has its
        // stop transition today, and it is the next event.
        for offset in -1..8 {
            let date = now.date_naive() + chrono::Duration::days(offset);
            if !self.days[weekday_index(date.weekday())] {
                continue;
            }
            let start_dt = date
                .and_time(start)
                .and_local_timezone(Local)
                .single()?;
            // The stop belongs to the next calendar day when the
            // window crosses midnight (or runs the whole day).
            let stop_date = if stop <= start {
                date + chrono::Duration::days(1)
            } else {
                date
            };
            let stop_dt = stop_date
                .and_time(stop)
                .and_local_timezone(Local)
                .single()?;
            for (kind, dt) in [(NextKind::Start, start_dt), (NextKind::Stop, stop_dt)] {
                if dt > now && best.map(|(_, b)| dt < b).unwrap_or(true) {
                    best = Some((kind, dt));
                }
            }
        }
        best
    }

    /// One human-readable line for the UI, e.g. "Queue starts today
    /// 23:00", "Queue stops tomorrow 06:00", "Queue starts Mon 23:00".
    pub fn next_label(&self, now: DateTime<Local>) -> Option<String> {
        let (kind, dt) = self.next_event(now)?;
        let verb = match kind {
            NextKind::Start => "starts",
            NextKind::Stop => "stops",
        };
        let today = now.date_naive();
        let day = if dt.date_naive() == today {
            "today".to_string()
        } else if dt.date_naive() == today + chrono::Duration::days(1) {
            "tomorrow".to_string()
        } else {
            short_weekday(dt.weekday()).to_string()
        };
        Some(format!(
            "Queue {verb} {day} {:02}:{:02}",
            dt.hour(),
            dt.minute()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-09 is a Monday.
    fn at(day: u32, hour: u32, min: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 11, day, hour, min, 0)
            .unwrap()
    }

    fn sched(start: &str, stop: &str, days: [bool; 7]) -> DailySchedule {
        DailySchedule {
            enabled: true,
            start: start.to_string(),
            stop: stop.to_string(),
            days,
        }
    }

    const ALL: [bool; 7] = [true; 7];
    const MON_ONLY: [bool; 7] = [true, false, false, false, false, false, false];

    #[test]
    fn daytime_window_inside_and_outside() {
        let s = sched("09:00", "17:00", ALL);
        assert!(s.contains(at(9, 10, 0)));
        assert!(s.contains(at(9, 9, 0)));
        assert!(!s.contains(at(9, 8, 59)));
        assert!(!s.contains(at(9, 17, 0)));
    }

    #[test]
    fn overnight_window_crosses_midnight() {
        let s = sched("23:00", "06:00", ALL);
        assert!(s.contains(at(9, 23, 30))); // Mon evening
        assert!(s.contains(at(10, 1, 0))); // Tue after midnight
        assert!(!s.contains(at(10, 7, 0)));
        assert!(!s.contains(at(9, 22, 59)));
    }

    #[test]
    fn overnight_window_belongs_to_start_day() {
        // Monday-only window: Tue 01:00 is still Monday's window,
        // Tue 23:00 is not (Tuesday is not selected).
        let s = sched("23:00", "06:00", MON_ONLY);
        assert!(s.contains(at(9, 23, 30)));
        assert!(s.contains(at(10, 1, 0)));
        assert!(!s.contains(at(10, 23, 30)));
        assert!(!s.contains(at(11, 1, 0))); // Wed 01:00: Tue's window, not selected
    }

    #[test]
    fn day_filter_blocks_unselected_days() {
        let s = sched("09:00", "17:00", MON_ONLY);
        assert!(s.contains(at(9, 10, 0))); // Mon
        assert!(!s.contains(at(10, 10, 0))); // Tue
    }

    #[test]
    fn start_equals_stop_is_whole_day() {
        let s = sched("09:00", "09:00", MON_ONLY);
        assert!(s.contains(at(9, 3, 0)));
        assert!(s.contains(at(9, 23, 59)));
        assert!(!s.contains(at(10, 3, 0)));
    }

    #[test]
    fn disabled_never_contains() {
        let mut s = sched("09:00", "17:00", ALL);
        s.enabled = false;
        assert!(!s.contains(at(9, 10, 0)));
        assert!(s.next_event(at(9, 10, 0)).is_none());
    }

    #[test]
    fn next_event_inside_window_is_stop() {
        let s = sched("23:00", "06:00", ALL);
        let (kind, dt) = s.next_event(at(9, 23, 30)).unwrap();
        assert_eq!(kind, NextKind::Stop);
        assert_eq!((dt.hour(), dt.minute()), (6, 0));
        assert_eq!(dt.day(), 10); // tomorrow
    }

    #[test]
    fn next_event_outside_window_is_start() {
        let s = sched("23:00", "06:00", ALL);
        let (kind, dt) = s.next_event(at(9, 10, 0)).unwrap();
        assert_eq!(kind, NextKind::Start);
        assert_eq!((dt.hour(), dt.minute(), dt.day()), (23, 0, 9));
    }

    #[test]
    fn next_event_skips_unselected_days() {
        // Monday-only, 09:00–17:00; it is Tue 10:00 -> next start is
        // next Monday.
        let s = sched("09:00", "17:00", MON_ONLY);
        let (kind, dt) = s.next_event(at(10, 10, 0)).unwrap();
        assert_eq!(kind, NextKind::Start);
        assert_eq!(dt.day(), 16); // next Monday
    }

    #[test]
    fn next_event_inside_overnight_window_started_yesterday() {
        // Monday-only 23:00–06:00; it is Tue 01:00, inside Monday's
        // window — the next event is its stop, today 06:00.
        let s = sched("23:00", "06:00", MON_ONLY);
        let (kind, dt) = s.next_event(at(10, 1, 0)).unwrap();
        assert_eq!(kind, NextKind::Stop);
        assert_eq!((dt.hour(), dt.minute(), dt.day()), (6, 0, 10));
        assert_eq!(
            s.next_label(at(10, 1, 0)).as_deref(),
            Some("Queue stops today 06:00")
        );
    }

    #[test]
    fn next_label_words() {
        let s = sched("23:00", "06:00", ALL);
        assert_eq!(
            s.next_label(at(9, 10, 0)).as_deref(),
            Some("Queue starts today 23:00")
        );
        assert_eq!(
            s.next_label(at(9, 23, 30)).as_deref(),
            Some("Queue stops tomorrow 06:00")
        );
        let mon = sched("09:00", "17:00", MON_ONLY);
        assert_eq!(
            mon.next_label(at(10, 10, 0)).as_deref(),
            Some("Queue starts Mon 09:00")
        );
    }

    #[test]
    fn malformed_times_fall_back_to_defaults() {
        let s = DailySchedule {
            enabled: true,
            start: "xx".to_string(),
            stop: String::new(),
            days: ALL,
        };
        let (start, stop) = s.window();
        assert_eq!((start.hour(), start.minute()), (23, 0));
        assert_eq!((stop.hour(), stop.minute()), (6, 0));
    }

    #[test]
    fn serde_round_trip_and_defaults() {
        let s: DailySchedule = serde_json::from_str(
            r#"{"enabled":true,"start":"22:30","stop":"05:00","days":[true,false,true,false,true,false,false]}"#,
        )
        .unwrap();
        assert!(s.enabled);
        assert_eq!(s.start, "22:30");
        assert!(!s.days[1]);
        // An old settings.json without the new fields still loads.
        let old: DailySchedule = serde_json::from_str("{}").unwrap();
        assert!(!old.enabled);
        assert_eq!(old.start, DEFAULT_START);
        assert_eq!(old.stop, DEFAULT_STOP);
        assert_eq!(old.days, [true; 7]);
    }
}
