//! 文件日志基础设施（P1/P2 共用）。
//!
//! 位置：`logs/app-YYYY-MM-DD.log`（按本地日期分文件）。
//! 格式：`[YYYY-MM-DD HH:MM:SS.mmm] LEVEL 消息`。
//! 写入失败静默降级到 stderr，绝不 panic；日志里严禁出现 apikey 明文。
//!
//! 用法（crate 内）：`crate::log_info!("配置已保存，has_key={}", true);`

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Local};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

struct Logger {
    dir: PathBuf,
    file: Option<File>,
    date: String,
}

impl Logger {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            file: None,
            date: String::new(),
        }
    }

    fn write_line(&mut self, now: &DateTime<Local>, line: &str) {
        let date = now.format("%Y-%m-%d").to_string();
        if self.file.is_none() || self.date != date {
            self.rotate(&date);
        }
        if let Some(file) = self.file.as_mut() {
            // 写失败不崩溃，也不重试别的目录
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
    }

    fn rotate(&mut self, date: &str) {
        if let Err(error) = std::fs::create_dir_all(&self.dir) {
            eprintln!("日志目录创建失败 {}: {error}", self.dir.display());
            self.file = None;
            self.date = date.to_string();
            return;
        }
        let path = self.dir.join(format!("app-{date}.log"));
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                self.file = Some(file);
                self.date = date.to_string();
            }
            Err(error) => {
                eprintln!("日志文件打开失败 {}: {error}", path.display());
                self.file = None;
                self.date = date.to_string();
            }
        }
    }
}

static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();

/// 初始化日志（幂等）。`logs_dir` 通常来自 `paths::get().logs_dir`。
pub fn init(logs_dir: &Path) {
    let _ = LOGGER.set(Mutex::new(Logger::new(logs_dir.to_path_buf())));
}

pub fn debug(message: impl AsRef<str>) {
    if cfg!(debug_assertions) {
        write(Level::Debug, message.as_ref());
    }
}

pub fn info(message: impl AsRef<str>) {
    write(Level::Info, message.as_ref());
}

pub fn warn(message: impl AsRef<str>) {
    write(Level::Warn, message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
    write(Level::Error, message.as_ref());
}

fn write(level: Level, message: &str) {
    let now = Local::now();
    let line = format!(
        "[{}] {:<5} {}\r\n",
        now.format("%Y-%m-%d %H:%M:%S%.3f"),
        level.as_str(),
        message
    );

    if let Some(lock) = LOGGER.get() {
        if let Ok(mut logger) = lock.lock() {
            logger.write_line(&now, &line);
            return;
        }
    }
    // 日志未初始化或锁中毒时兜底到 stderr
    eprintln!("{}", line.trim_end());
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::logging::debug(format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::logging::info(format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::logging::warn(format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::logging::error(format!($($arg)*))
    };
}
