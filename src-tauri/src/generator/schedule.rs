//! 07:00 定时判定、补跑日期计算、可注入时钟。
//!
//! 契约（任务书 R1/R2/R10）：
//! - 07:00 与补跑日期一律按本地时间（Asia/Shanghai），不用 UTC 判定日期；
//! - 07:00 目标日期用 P0 `day::previous_day_key`（调用方负责），这里只负责时刻计算；
//! - 补跑判定：某天 D 的「次日 07:00」已过，且 `output/D.md` 缺失；
//! - 短轮询（15–20s）而不是一次长睡，以扛休眠/唤醒与系统时间跳变。

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Local, LocalResult, NaiveDate, NaiveTime, TimeZone};

/// 每日生成时刻：本地 07:00。
pub const GENERATION_HOUR: u32 = 7;

/// 定时线程轮询间隔。短轮询让休眠唤醒/系统时间跳变后仍能在 ~20s 内发现「07:00 已过」。
pub const SCHEDULE_POLL_INTERVAL: Duration = Duration::from_secs(20);

/// 可注入时钟（测试用假时钟，不真的睡眠）。
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Local>;
    fn sleep(&self, duration: Duration);
}

/// 生产用系统时钟。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Local> {
        Local::now()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

fn generation_time() -> NaiveTime {
    // 07:00 恒定合法，不会 panic
    NaiveTime::from_hms_opt(GENERATION_HOUR, 0, 0).expect("07:00 是合法时间")
}

/// 某本地日历日的 07:00；夏令时跳变导致该本地时间不存在时返回 `None`。
pub fn seven_am_on(date: NaiveDate) -> Option<DateTime<Local>> {
    let naive = date.and_time(generation_time());
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(value) => Some(value),
        LocalResult::Ambiguous(first, _second) => Some(first),
        LocalResult::None => None,
    }
}

/// 仅用于「同一日历日只触发一次」的去重标记，**不是** `day_key`。
pub fn calendar_day(now: DateTime<Local>) -> String {
    now.format("%Y-%m-%d").to_string()
}

/// 严格解析 `YYYY-MM-DD`（回写后必须与输入完全一致，拒绝 `2026-1-1` 之类的变体）。
pub fn parse_day_key(value: &str) -> Option<NaiveDate> {
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    if date.format("%Y-%m-%d").to_string() == value {
        Some(date)
    } else {
        None
    }
}

/// 补跑截止时刻：`day` 这一天的**次日** 07:00。
pub fn backfill_cutoff(day: &str) -> Option<DateTime<Local>> {
    let date = parse_day_key(day)?;
    let next = date.checked_add_signed(ChronoDuration::days(1))?;
    seven_am_on(next)
}

/// 该天是否已到补跑时间（次日 07:00 已过）。
pub fn is_backfill_due(now: DateTime<Local>, day: &str) -> bool {
    matches!(backfill_cutoff(day), Some(cutoff) if now >= cutoff)
}

/// 补跑对象：`data_dir` 中「次日 07:00 已过」且 `output_dir` 缺对应 md 的日期，按日期升序。
///
/// 只读目录；读不到目录（不存在/无权限）时返回空列表，由调用方记日志。
pub fn backfill_days(data_dir: &Path, output_dir: &Path, now: DateTime<Local>) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(data_dir) else {
        return Vec::new();
    };

    let mut candidates: Vec<(NaiveDate, String)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(date) = parse_day_key(stem) else {
            continue;
        };
        if output_dir.join(format!("{stem}.md")).exists() {
            continue;
        }
        if !is_backfill_due(now, stem) {
            continue;
        }
        candidates.push((date, stem.to_string()));
    }

    candidates.sort_by(|left, right| left.0.cmp(&right.0));
    candidates.into_iter().map(|(_date, day)| day).collect()
}
