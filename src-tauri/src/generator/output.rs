//! 记录读取、旁白校验、去代码围栏、截断、落盘。
//!
//! 契约（任务书 R3/R7/R14/R15/R16）：
//! - 读取 `data/{day}.md`，用 P0 `record_line::parse_line` 逐行解析；空行/非法行跳过；
//! - 标题行（`行首 0–3 空格 + #×1..6 + 一个空格 + 文字`）按结构行跳过，不参与解析、不产生 WARN、
//!   不计入记录条数、不进入 prompt（R15）；只有标题没有记录时仍按 0 条处理；
//! - 有效记录 0 条 → 调用方静默跳过；
//! - 旁白正文非空；按 Unicode 字符数 ≤800（R16 硬上限，含标点空格）；
//! - **不校验时间戳**（R16）：v3.0 不要求正文出现时间戳，也不因缺失/记录外时间戳触发重试；
//!   `collect_timestamps` 仅用于日志统计「正文含时间戳 n 处」；
//! - 输出文件只写正文本身（不加尾随换行，避免「文件 801 字」）。

use std::path::Path;

use crate::record_line::{parse_line, Entry};

/// 旁白字数硬上限（Unicode 字符数，R16：v3.0 起为 800）。
pub const MAX_CHARS: usize = 800;

/// 当天记录：解析后的条目 + 用于 prompt 的原文行。
#[derive(Debug, Clone)]
pub struct DayRecords {
    pub entries: Vec<Entry>,
    /// 解析成功的原始行（trim 后），按文件顺序，用于 prompt 用户消息。
    pub lines: Vec<String>,
    pub total_lines: usize,
    /// 标题行条数（R15：结构行，不计入记录条数，也不进 prompt）。
    pub heading_lines: usize,
    pub invalid_lines: usize,
}

/// `read_day` 的三种结果。
#[derive(Debug)]
pub enum ReadOutcome {
    Read(DayRecords),
    /// 记录文件不存在（含补跑期间被删除）
    Missing,
    /// 读取失败（权限/编码等）
    Failed(String),
}

/// 读取并解析一天的记录文件；不 panic、不写任何文件。
pub fn read_day(path: &Path) -> ReadOutcome {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return ReadOutcome::Missing,
        Err(error) => return ReadOutcome::Failed(format!("读取记录文件失败: {error}")),
    };

    let mut entries: Vec<Entry> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut total_lines = 0usize;
    let mut heading_lines = 0usize;
    let mut invalid_lines = 0usize;

    for line in raw.lines() {
        total_lines += 1;
        // R15：整页文档编辑器允许 Markdown 标题；按结构行跳过（不是错误，不写 WARN、不计条数）。
        if is_heading_line(line) {
            heading_lines += 1;
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            invalid_lines += 1;
            continue;
        }
        match parse_line(trimmed) {
            Ok(entry) => {
                entries.push(entry);
                lines.push(trimmed.to_string());
            }
            Err(_error) => invalid_lines += 1,
        }
    }

    ReadOutcome::Read(DayRecords {
        entries,
        lines,
        total_lines,
        heading_lines,
        invalid_lines,
    })
}

/// 判断一行是否是 Markdown 标题行（R15）。
///
/// 格式：行首 0–3 个空格 + `#`×1..6 + **一个**空格 + 非空文字。
/// 4 个及以上前导空格的 `#` 行、`#` 后无空格、`#` 后只有空白，都不算标题行（按普通行处理）。
pub fn is_heading_line(line: &str) -> bool {
    let bytes = line.as_bytes();

    let mut index = 0usize;
    while index < bytes.len() && index < 3 && bytes[index] == b' ' {
        index += 1;
    }
    // 前导空格超过 3 个 → 不是标题行
    if index < bytes.len() && bytes[index] == b' ' {
        return false;
    }

    let hashes_start = index;
    while index < bytes.len() && bytes[index] == b'#' {
        index += 1;
    }
    let hashes = index - hashes_start;
    if !(1..=6).contains(&hashes) {
        return false;
    }

    // 标题标记后必须紧跟一个空格，且空格后要有文字
    if index >= bytes.len() || bytes[index] != b' ' {
        return false;
    }
    !line[index + 1..].trim().is_empty()
}

/// 校验结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validation {
    Ok { text: String, chars: usize },
    /// 正文为空/全空白（R14：视为失败）
    Empty,
    /// 超过 800 字符（R16：重试一次更严格指令，仍超则截断兜底）
    TooLong { text: String, chars: usize },
}

impl Validation {
    /// 面向日志/更严格指令的失败原因（不含记录原文、不含 prompt 全文）。
    pub fn retry_reason(&self) -> String {
        match self {
            Validation::Ok { .. } => "通过".to_string(),
            Validation::Empty => "正文为空".to_string(),
            Validation::TooLong { chars, .. } => {
                format!("正文 {chars} 字符，超过 {MAX_CHARS} 字符上限")
            }
        }
    }
}

/// 校验模型返回的旁白：先去代码围栏/首尾空白，再查空、字数。
///
/// R16：不再校验时间戳——正文有没有 `HH:MM`、是否是记录里出现过的，都不影响通过与否。
pub fn validate(raw: &str) -> Validation {
    let text = clean(raw);
    if text.is_empty() {
        return Validation::Empty;
    }

    let chars = text.chars().count();
    if chars > MAX_CHARS {
        return Validation::TooLong { text, chars };
    }

    Validation::Ok { text, chars }
}

/// 去掉首尾空白与最外层的 Markdown 代码围栏。
pub fn clean(raw: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed.contains("```") {
        return trimmed.to_string();
    }

    let mut lines: Vec<&str> = trimmed.lines().collect();
    if let Some(first) = lines.first() {
        if first.trim_start().starts_with("```") {
            lines.remove(0);
        }
    }
    if let Some(last) = lines.last() {
        if last.trim_start().starts_with("```") {
            lines.pop();
        }
    }
    lines.join("\n").trim().to_string()
}

/// 截断到 800 字符以内（Unicode 字符数），并去掉尾部空白。
pub fn truncate_to_limit(text: &str) -> String {
    text.trim()
        .chars()
        .take(MAX_CHARS)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// 扫描正文中所有 `HH:MM` 形态的时间戳（两位时:两位分，左右不能再连着数字）。
///
/// R16：结果**只用于日志统计**（「正文含时间戳 n 处」），不再参与校验，也不比对记录。
/// 不做 0–23 / 0–59 范围过滤：`24:00` 这类写法同样会被数到。
pub fn collect_timestamps(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut stamps: Vec<String> = Vec::new();
    let mut index = 0usize;

    while index + 4 < bytes.len() {
        let candidate = bytes[index].is_ascii_digit()
            && bytes[index + 1].is_ascii_digit()
            && bytes[index + 2] == b':'
            && bytes[index + 3].is_ascii_digit()
            && bytes[index + 4].is_ascii_digit();

        if candidate {
            let left_boundary = index == 0 || !bytes[index - 1].is_ascii_digit();
            let right_boundary = index + 5 >= bytes.len() || !bytes[index + 5].is_ascii_digit();
            if left_boundary && right_boundary {
                stamps.push(text[index..index + 5].to_string());
            }
            index += 5;
        } else {
            index += 1;
        }
    }

    stamps
}

/// 写输出文件：只在所有校验通过后调用；不写尾随换行。
///
/// 原子写（R16「保留原子写」）：先写同目录临时文件 `*.md.tmp`，再 rename 到目标；
/// 失败时清理临时文件，绝不留下半个文件。调用方已先做幂等检查，目标不应已存在。
pub fn write_output(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("创建输出目录失败: {error}"))?;
    }

    let temp_path = path.with_extension("md.tmp");
    std::fs::write(&temp_path, text.as_bytes())
        .map_err(|error| format!("写入临时输出文件失败: {error}"))?;

    if let Err(error) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!("替换输出文件失败: {error}"));
    }

    Ok(())
}
