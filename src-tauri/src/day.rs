//! 「一天」的定义：本地时间当天 06:00 到次日 06:00；0:00–5:59 归前一天。
//!
//! 全部使用本地时区（Asia/Shanghai），不用 UTC 做日期判定。

use chrono::{DateTime, Duration, Local, LocalResult, NaiveTime, TimeZone};

/// 跨天边界：早上 6 点。
pub const DAY_BOUNDARY_HOUR: u32 = 6;

fn boundary_time() -> NaiveTime {
    // 06:00 恒定合法，不会 panic
    NaiveTime::from_hms_opt(DAY_BOUNDARY_HOUR, 0, 0).expect("06:00 是合法时间")
}

/// 某个时刻属于哪一天：返回 `YYYY-MM-DD`（这天开始的日期）。
pub fn day_key(now: DateTime<Local>) -> String {
    let local = now.naive_local();
    let date = if local.time() < boundary_time() {
        local.date() - Duration::days(1)
    } else {
        local.date()
    };
    date.format("%Y-%m-%d").to_string()
}

/// 上一个「已结束」的天的 key。
pub fn previous_day_key(now: DateTime<Local>) -> String {
    day_key(now - Duration::days(1))
}

/// 严格晚于 `now` 的下一个 06:00（用于 `day-changed` 定时）。
pub fn next_boundary_after(now: DateTime<Local>) -> DateTime<Local> {
    let local = now.naive_local();
    let today = local.date();
    let boundary_today = today.and_time(boundary_time());

    let target = if local < boundary_today {
        boundary_today
    } else {
        (today + Duration::days(1)).and_time(boundary_time())
    };

    match Local.from_local_datetime(&target) {
        LocalResult::Single(datetime) => datetime,
        LocalResult::Ambiguous(first, _second) => first,
        // 夏令时跳变等极端情况：兜底一小时后重试
        LocalResult::None => now + Duration::hours(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Local, Timelike, TimeZone};

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("本地时间应存在")
    }

    #[test]
    fn minute_before_boundary_belongs_to_previous_day() {
        assert_eq!(day_key(at(2026, 11, 15, 0, 30)), "2026-11-14");
        assert_eq!(day_key(at(2026, 11, 15, 5, 59)), "2026-11-14");
    }

    #[test]
    fn boundary_at_0600_starts_a_new_day() {
        assert_eq!(day_key(at(2026, 11, 15, 6, 0)), "2026-11-15");
        assert_eq!(day_key(at(2026, 11, 15, 23, 59)), "2026-11-15");
    }

    #[test]
    fn day_key_uses_local_date_not_utc_date() {
        // 本地 11-15 06:00 对应 UTC 11-14 22:00；必须按本地日期判定
        assert_eq!(day_key(at(2026, 11, 15, 6, 0)), "2026-11-15");
    }

    #[test]
    fn previous_day_key_shifts_one_day() {
        assert_eq!(previous_day_key(at(2026, 11, 16, 7, 0)), "2026-11-15");
        assert_eq!(previous_day_key(at(2026, 12, 1, 7, 0)), "2026-11-30");
        assert_eq!(previous_day_key(at(2026, 12, 1, 3, 0)), "2026-11-29");
    }

    #[test]
    fn next_boundary_is_strictly_in_the_future() {
        let early = at(2026, 11, 15, 5, 0);
        let next = next_boundary_after(early);
        assert!(next > early);
        assert_eq!(next.hour(), 6);
        assert_eq!(next.day(), 15);

        let late = at(2026, 11, 15, 6, 0);
        let next = next_boundary_after(late);
        assert!(next > late);
        assert_eq!(next.hour(), 6);
        assert_eq!(next.day(), 16);
    }
}
