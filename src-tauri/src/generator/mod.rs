//! 生成引擎（P2 实现）：07:00 定时、启动补跑、DeepSeek 调用、校验、落盘、屏底提示。
//!
//! 挂载点：`lib.rs` 在应用启动时调用一次 [`init`]。
//!
//! 契约要点（任务书 R1–R16）：
//! - 每天本地 07:00 处理「昨天」，同一进程同一日历日只触发一次；
//! - 启动时补跑「次日 07:00 已过且缺 output」的天，逐天独立成败、默认不弹提示；
//! - 0 条记录 / 无 key / 失败 / output 已存在 → 静默跳过 + 写日志；
//! - 只有全部校验通过才写 `output/{day}.md`，只有 07:00 定时成功才弹屏底提示；
//! - R16：旁白提示词 v3.0（日常小说叙事），字数上限 800 字符，不再校验时间戳。
//!
//! 可测试性（R12）：路径、配置、HTTP 客户端、时钟、日志、提示全部可注入；
//! 生产实现分别是 `paths` / `Config` / reqwest / 系统时钟 / P0 日志宏 / `toast::show_toast`。

mod ai;
mod output;
mod prompt;
mod schedule;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tauri::AppHandle;

use crate::config::Config;
use crate::day;
use crate::paths;

use ai::{ChatError, ChatRequest};
use schedule::Clock;

/// 生成成功后的屏底提示文案（策划案 4.4 / 7.4）。
pub const TOAST_MESSAGE: &str = "今天的旁白写好了";

// ---------------------------------------------------------------------------
// 可注入依赖：日志 / 提示 / 配置
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

/// 日志出口；生产走 P0 `logging`（任务书 R9）。
pub trait LogSink: Send + Sync {
    fn write(&self, level: LogLevel, message: &str);
}

/// P0 日志宏实现（写 `logs/app-YYYY-MM-DD.log`）。
pub struct P0LogSink;

impl LogSink for P0LogSink {
    fn write(&self, level: LogLevel, message: &str) {
        match level {
            LogLevel::Info => crate::log_info!("{}", message),
            LogLevel::Warn => crate::log_warn!("{}", message),
            LogLevel::Error => crate::log_error!("{}", message),
        }
    }
}

/// 屏底提示出口；生产调用 P0 `toast::show_toast`（任务书 R8）。
pub trait Notifier: Send + Sync {
    fn notify(&self, message: &str);
}

/// 生产实现：从后台线程切到主线程再调 P0 的 `toast::show_toast`。
pub struct AppNotifier {
    app: AppHandle,
}

impl Notifier for AppNotifier {
    fn notify(&self, message: &str) {
        let app = self.app.clone();
        let message = message.to_string();
        let scheduled = self.app.run_on_main_thread(move || {
            if let Err(error) = crate::toast::show_toast(&app, &message) {
                crate::log_error!("屏底提示发送失败: {}", error);
            }
        });
        if let Err(error) = scheduled {
            crate::log_error!("屏底提示调度失败: {}", error);
        }
    }
}

/// 生成所需的配置快照（只拷 key/baseURL/model，不写日志）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenConfig {
    pub api_key: Option<String>,
    pub base_url: String,
    pub model: String,
}

impl GenConfig {
    pub fn from_config(config: &Config) -> Self {
        Self {
            api_key: config.api_key(),
            base_url: config.base_url.clone(),
            model: config.model.clone(),
        }
    }
}

/// 配置来源；每次生成时重新读取，用户中途保存 key 后下一次生成即可用。
pub trait ConfigSource: Send + Sync {
    fn load(&self) -> GenConfig;
}

/// 生产实现：P0 `Config::load()`（只读读取，不打印 key）。
pub struct P0ConfigSource;

impl ConfigSource for P0ConfigSource {
    fn load(&self) -> GenConfig {
        GenConfig::from_config(&Config::load())
    }
}

// ---------------------------------------------------------------------------
// 路径与结果类型
// ---------------------------------------------------------------------------

/// 生成引擎用到的两个目录（测试注入临时目录，不碰用户真实数据）。
#[derive(Debug, Clone)]
pub struct GenPaths {
    pub data_dir: PathBuf,
    pub output_dir: PathBuf,
}

impl GenPaths {
    pub fn from_app_paths(app_paths: &paths::AppPaths) -> Self {
        Self {
            data_dir: app_paths.data_dir.clone(),
            output_dir: app_paths.output_dir.clone(),
        }
    }

    pub fn data_file(&self, day: &str) -> PathBuf {
        self.data_dir.join(format!("{day}.md"))
    }

    pub fn output_file(&self, day: &str) -> PathBuf {
        self.output_dir.join(format!("{day}.md"))
    }
}

/// 触发来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// 每日 07:00 定时
    Scheduled,
    /// 启动补跑
    Backfill,
}

impl Trigger {
    pub fn label(self) -> &'static str {
        match self {
            Trigger::Scheduled => "定时",
            Trigger::Backfill => "补跑",
        }
    }
}

/// 静默跳过原因（不弹提示、不写 output）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// output 已存在（幂等，R7）
    OutputExists,
    /// 有效记录 0 条（R3/R14）
    NoRecords,
    /// 记录文件不存在（含补跑期间被删除，R14）
    DataMissing,
    /// 未配置 API key（R4）
    NoKey,
    /// 读取记录文件失败（R3）
    ReadError(String),
    /// 本进程已尝试过该日期（R11 去重标记）
    AlreadyAttempted,
}

impl SkipReason {
    pub fn label(&self) -> String {
        match self {
            SkipReason::OutputExists => "output 已存在（幂等跳过）".to_string(),
            SkipReason::NoRecords => "记录 0 条".to_string(),
            SkipReason::DataMissing => "记录文件不存在".to_string(),
            SkipReason::NoKey => "未配置 API key".to_string(),
            SkipReason::ReadError(error) => format!("读取记录失败: {error}"),
            SkipReason::AlreadyAttempted => "本进程已尝试过该日期".to_string(),
        }
    }
}

/// 单天生成的最终结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Written {
        day: String,
        chars: usize,
        calls: u32,
        elapsed_ms: u128,
    },
    Skipped {
        day: String,
        reason: SkipReason,
    },
    Failed {
        day: String,
        reason: String,
        calls: u32,
        elapsed_ms: u128,
    },
}

/// 重试策略：3 次尝试，间隔 2s / 4s 递增，总等待有上限。
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(10),
        }
    }
}

impl RetryPolicy {
    /// 第 `attempt` 次尝试失败后、下一次尝试前的等待（2s、4s……不超过 `max_delay`）。
    pub fn delay_after(&self, attempt: u32) -> Duration {
        let factor = 1u32 << attempt.saturating_sub(1).min(4);
        (self.base_delay * factor).min(self.max_delay)
    }
}

// ---------------------------------------------------------------------------
// 引擎
// ---------------------------------------------------------------------------

/// 进程内状态：串行化生成、去重标记（R11）。
#[derive(Default)]
struct EngineState {
    /// 本进程已开始生成（已调用 API）的日期。
    attempted_days: HashSet<String>,
    /// 本进程已触发过 07:00 的本地日历日。
    fired_calendar_days: HashSet<String>,
}

pub struct Engine {
    paths: GenPaths,
    config: Arc<dyn ConfigSource>,
    client: Arc<dyn ai::ChatClient>,
    clock: Arc<dyn Clock>,
    log: Arc<dyn LogSink>,
    notifier: Arc<dyn Notifier>,
    retry: RetryPolicy,
    state: Mutex<EngineState>,
}

impl Engine {
    /// 串行化生成：整个「读记录 → 调 API → 写文件」都在锁内，重复触发不会并发调用 API。
    fn lock_state(&self) -> MutexGuard<'_, EngineState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn info(&self, message: impl AsRef<str>) {
        self.log.write(LogLevel::Info, message.as_ref());
    }

    fn warn(&self, message: impl AsRef<str>) {
        self.log.write(LogLevel::Warn, message.as_ref());
    }

    fn error(&self, message: impl AsRef<str>) {
        self.log.write(LogLevel::Error, message.as_ref());
    }

    /// 启动补跑（R2）：逐天生成，单天失败只记日志并继续；默认不弹屏底提示。
    ///
    /// 返回按日期升序的结果列表，便于测试与汇报；无待补日期时返回空列表并静默。
    pub fn run_startup_backfill(&self) -> Vec<(String, Outcome)> {
        let now = self.clock.now();
        let days = schedule::backfill_days(&self.paths.data_dir, &self.paths.output_dir, now);

        if days.is_empty() {
            self.info("[补跑] 启动检查：无待补日期");
            return Vec::new();
        }
        self.info(format!("[补跑] 启动检查：待补 {} 天（{}）", days.len(), days.join("、")));

        let config = self.config.load();
        if config.api_key.is_none() {
            self.warn("[补跑] 未配置 API key，整批补跑跳过（静默）");
            return Vec::new();
        }

        let mut results: Vec<(String, Outcome)> = Vec::new();
        for day in days {
            let outcome = self.generate_day(&day, Trigger::Backfill);
            results.push((day, outcome));
        }

        let written = results
            .iter()
            .filter(|(_day, outcome)| matches!(outcome, Outcome::Written { .. }))
            .count();
        self.info(format!("[补跑] 结束：成功 {written}/{} 天", results.len()));
        results
    }

    /// 07:00 定时判定（R1）：同一进程同一本地日历日只触发一次，目标日期为「昨天」。
    ///
    /// 返回 `Some((目标日期, 结果))` 表示本次触发了；`None` 表示未到点或今日已触发。
    pub fn tick(&self) -> Option<(String, Outcome)> {
        let now = self.clock.now();
        let seven = schedule::seven_am_on(now.date_naive())?;
        if now < seven {
            return None;
        }

        let calendar = schedule::calendar_day(now);
        {
            let mut state = self.lock_state();
            if !state.fired_calendar_days.insert(calendar.clone()) {
                return None;
            }
        }

        let target_day = day::previous_day_key(now);
        self.info(format!("[定时] 07:00 触发（日历日 {calendar}，目标日期 {target_day}）"));
        let outcome = self.generate_day(&target_day, Trigger::Scheduled);
        Some((target_day, outcome))
    }

    /// 后台线程主循环：先补跑，再按短轮询检查 07:00。
    pub fn run_forever(&self) -> ! {
        self.run_startup_backfill();
        loop {
            self.tick();
            self.clock.sleep(schedule::SCHEDULE_POLL_INTERVAL);
        }
    }

    /// 生成某一天（R3–R8）：幂等检查 → 读记录 → 调 API（3 次尝试）→ 校验 → 可能一轮更严格重试 → 落盘 → 提示。
    pub fn generate_day(&self, day: &str, trigger: Trigger) -> Outcome {
        let started = Instant::now();

        // 串行锁：同一时刻只允许一个生成在跑（R11）。
        // 注意：日志/提示/HTTP 都不回调 Engine 的 state，因此持锁期间不会死锁。
        let mut state = self.lock_state();

        if state.attempted_days.contains(day) {
            let reason = SkipReason::AlreadyAttempted;
            self.info(format!("[{}] {day} 跳过：{}", trigger.label(), reason.label()));
            return Outcome::Skipped {
                day: day.to_string(),
                reason,
            };
        }

        let output_path = self.paths.output_file(day);
        if output_path.exists() {
            let reason = SkipReason::OutputExists;
            self.info(format!("[{}] {day} 跳过：{}", trigger.label(), reason.label()));
            return Outcome::Skipped {
                day: day.to_string(),
                reason,
            };
        }

        let records = match output::read_day(&self.paths.data_file(day)) {
            output::ReadOutcome::Read(records) => records,
            output::ReadOutcome::Missing => {
                let reason = SkipReason::DataMissing;
                self.warn(format!("[{}] {day} 跳过：{}", trigger.label(), reason.label()));
                return Outcome::Skipped {
                    day: day.to_string(),
                    reason,
                };
            }
            output::ReadOutcome::Failed(error) => {
                let reason = SkipReason::ReadError(error.clone());
                self.error(format!("[{}] {day} 跳过：{}", trigger.label(), reason.label()));
                return Outcome::Skipped {
                    day: day.to_string(),
                    reason,
                };
            }
        };

        // R15：标题行是结构行，静默跳过；只记 INFO，绝不写 WARN。
        if records.heading_lines > 0 {
            self.info(format!(
                "[{}] {day} 跳过标题行 {} 条（结构行，不计入记录条数）",
                trigger.label(),
                records.heading_lines
            ));
        }

        if records.entries.is_empty() {
            let reason = SkipReason::NoRecords;
            self.info(format!(
                "[{}] {day} 跳过：{}（共 {} 行，标题行 {} 条，空行/非法行 {} 条）",
                trigger.label(),
                reason.label(),
                records.total_lines,
                records.heading_lines,
                records.invalid_lines
            ));
            return Outcome::Skipped {
                day: day.to_string(),
                reason,
            };
        }

        let config = self.config.load();
        let Some(api_key) = config.api_key.clone() else {
            let reason = SkipReason::NoKey;
            self.warn(format!("[{}] {day} 跳过：{}", trigger.label(), reason.label()));
            return Outcome::Skipped {
                day: day.to_string(),
                reason,
            };
        };

        // 真正要调 API 了才标记「本进程已尝试」，避免 07:00 分支在补跑刚失败后立刻再打一遍。
        state.attempted_days.insert(day.to_string());

        self.info(format!(
            "[{}] {day} 开始生成（记录 {} 条，共 {} 行）",
            trigger.label(),
            records.entries.len(),
            records.total_lines
        ));

        let prompts = prompt::PromptPair::build(day, &records.lines);
        let mut calls: u32 = 0;

        // ---- 第 1 轮：常规指令，最多 3 次 HTTP 尝试 ----
        let first_text = match self.call_round(
            &config,
            &api_key,
            &prompts.system,
            &prompts.user,
            1,
            day,
            trigger,
            &mut calls,
        ) {
            Ok(text) => text,
            Err(error) => {
                return self.fail(
                    day,
                    trigger,
                    calls,
                    started,
                    format!(
                        "第 1 轮 {} 次尝试均失败（最后错误：{}）",
                        self.retry.attempts,
                        error.summary()
                    ),
                );
            }
        };

        match output::validate(&first_text) {
            output::Validation::Ok { text, chars } => {
                return self.finish(day, trigger, &text, chars, calls, started, None);
            }
            other => {
                let reason = other.retry_reason();
                self.warn(format!(
                    "[{}] {day} 第 1 轮输出未通过校验：{reason}；进入第 2 轮（更严格指令）",
                    trigger.label()
                ));

                let stricter_system = prompt::stricter_system(&prompts.system, &reason);
                let second_text = match self.call_round(
                    &config,
                    &api_key,
                    &stricter_system,
                    &prompts.user,
                    2,
                    day,
                    trigger,
                    &mut calls,
                ) {
                    Ok(text) => text,
                    Err(error) => {
                        return self.fail(
                            day,
                            trigger,
                            calls,
                            started,
                            format!(
                                "第 2 轮 {} 次尝试均失败（最后错误：{}）",
                                self.retry.attempts,
                                error.summary()
                            ),
                        );
                    }
                };

                match output::validate(&second_text) {
                    output::Validation::Ok { text, chars } => {
                        self.finish(day, trigger, &text, chars, calls, started, None)
                    }
                    output::Validation::TooLong { text, chars } => {
                        let truncated = output::truncate_to_limit(&text);
                        let truncated_chars = truncated.chars().count();
                        self.warn(format!(
                            "[{}] {day} 第 2 轮仍超限（{chars} 字 > {} 字），截断到 {truncated_chars} 字兜底",
                            trigger.label(),
                            output::MAX_CHARS
                        ));
                        self.finish(
                            day,
                            trigger,
                            &truncated,
                            truncated_chars,
                            calls,
                            started,
                            Some("截断兜底"),
                        )
                    }
                    other => self.fail(
                        day,
                        trigger,
                        calls,
                        started,
                        format!("第 2 轮仍未通过校验：{}", other.retry_reason()),
                    ),
                }
            }
        }
    }

    /// 一轮调用：最多 `retry.attempts` 次尝试，间隔递增；日志区分「第几轮」与「第几次尝试」。
    fn call_round(
        &self,
        config: &GenConfig,
        api_key: &str,
        system: &str,
        user: &str,
        round: u32,
        day: &str,
        trigger: Trigger,
        calls: &mut u32,
    ) -> Result<String, ChatError> {
        let request = ChatRequest {
            base_url: config.base_url.clone(),
            model: config.model.clone(),
            api_key: api_key.to_string(),
            system: system.to_string(),
            user: user.to_string(),
        };

        let mut last_error: Option<ChatError> = None;

        for attempt in 1..=self.retry.attempts {
            *calls += 1;
            match self.client.complete(&request) {
                Ok(text) => {
                    self.info(format!(
                        "[{}] {day} 第 {round} 轮第 {attempt}/{} 次 HTTP 尝试成功（返回 {} 字符）",
                        trigger.label(),
                        self.retry.attempts,
                        text.chars().count()
                    ));
                    return Ok(text);
                }
                Err(error) => {
                    self.warn(format!(
                        "[{}] {day} 第 {round} 轮第 {attempt}/{} 次 HTTP 尝试失败：{}",
                        trigger.label(),
                        self.retry.attempts,
                        error.summary()
                    ));
                    last_error = Some(error);
                    if attempt < self.retry.attempts {
                        self.clock.sleep(self.retry.delay_after(attempt));
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| ChatError::Shape("没有可用响应".to_string())))
    }

    /// 校验通过后落盘；仅 07:00 定时成功才弹屏底提示（先写文件，再提示）。
    fn finish(
        &self,
        day: &str,
        trigger: Trigger,
        text: &str,
        chars: usize,
        calls: u32,
        started: Instant,
        note: Option<&str>,
    ) -> Outcome {
        let output_path = self.paths.output_file(day);
        let elapsed_ms = started.elapsed().as_millis();

        if let Err(error) = output::write_output(&output_path, text) {
            self.error(format!(
                "[{}] {day} 写入失败：{error}（HTTP 调用 {calls} 次，耗时 {elapsed_ms}ms）",
                trigger.label()
            ));
            return Outcome::Failed {
                day: day.to_string(),
                reason: error,
                calls,
                elapsed_ms,
            };
        }

        let note_suffix = match note {
            Some(note) => format!("，{note}"),
            None => String::new(),
        };
        self.info(format!(
            "[{}] {day} 完成：写入 {}（{chars} 字，HTTP 调用 {calls} 次，耗时 {elapsed_ms}ms{note_suffix}）",
            trigger.label(),
            output_path.display()
        ));

        // R16：时间戳不再参与校验，只统计处数便于排查（出现记录外时间戳也不重试）。
        let stamps = output::collect_timestamps(text);
        self.info(format!(
            "[{}] {day} 正文含时间戳 {} 处（v3.0 不要求、不校验来源）",
            trigger.label(),
            stamps.len()
        ));

        if trigger == Trigger::Scheduled {
            self.notifier.notify(TOAST_MESSAGE);
            self.info(format!("[定时] {day} 已触发屏底提示：{TOAST_MESSAGE}"));
        }

        Outcome::Written {
            day: day.to_string(),
            chars,
            calls,
            elapsed_ms,
        }
    }

    fn fail(
        &self,
        day: &str,
        trigger: Trigger,
        calls: u32,
        started: Instant,
        reason: String,
    ) -> Outcome {
        let elapsed_ms = started.elapsed().as_millis();
        self.error(format!(
            "[{}] {day} 放弃：{reason}（HTTP 调用 {calls} 次，耗时 {elapsed_ms}ms）",
            trigger.label()
        ));
        Outcome::Failed {
            day: day.to_string(),
            reason,
            calls,
            elapsed_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// 挂载点
// ---------------------------------------------------------------------------

/// 应用启动时调用一次：起一个后台线程做「启动补跑 + 07:00 短轮询」。
///
/// 本函数只做接线，不做任何阻塞工作；线程内失败只写日志，不影响应用启动。
pub fn init(app: &AppHandle) {
    let app_paths = paths::get();

    let engine = Arc::new(Engine {
        paths: GenPaths::from_app_paths(app_paths),
        config: Arc::new(P0ConfigSource),
        client: Arc::new(ai::ReqwestChatClient::new()),
        clock: Arc::new(schedule::SystemClock),
        log: Arc::new(P0LogSink),
        notifier: Arc::new(AppNotifier { app: app.clone() }),
        retry: RetryPolicy::default(),
        state: Mutex::new(EngineState::default()),
    });

    crate::log_info!("生成引擎初始化：启动补跑 + 07:00 定时（后台线程）");

    let worker = engine.clone();
    match std::thread::Builder::new()
        .name("generator".to_string())
        .spawn(move || worker.run_forever())
    {
        Ok(_handle) => {}
        Err(error) => crate::log_error!("生成引擎线程启动失败: {}", error),
    }
}
