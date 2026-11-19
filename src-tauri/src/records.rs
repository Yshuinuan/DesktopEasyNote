//! 记录（P1 实现）。
//!
//! v1.5 R12：页面是「整页文档」，不再有单行/多行输入框。当天文件是这篇文档的
//! 落盘形式：
//! - 记录行：`HH:MM · 内容[ · 心情]`（沿用 P0 `record_line::format_line`）；
//! - 标题行：`#` 到 `######` + 空格 + 文字（1–6 级，无时间戳）。
//!
//! 命令：
//! - `get_today_document()`：读取当天文档（标题行 + 记录行，按文件顺序）；
//! - `save_today_document(lines, day?)`：整篇写回；`time: null` 的记录行由 Rust
//!   用同一个本地 `HH:MM` 回填；`day = "previous"` 时写前一天文件（仅用于 06:00
//!   `day-changed` 先落旧一天）。整篇用「同目录临时文件 + 原子替换」写入，失败时
//!   保留上一版文件，不出现半篇内容。
//!
//! 日期/时间戳一律由 Rust 按本地时间计算；文件路径一律走 P0 `paths::get()`；
//! 记录行格式与解析一律走 P0 `record_line`。测试全部使用临时目录。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::paths::{self, AppPaths};
use crate::record_line::{self, Mood};

/// 单进程内串行化整篇写回，避免并发保存互相覆盖。
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 临时文件序号，保证同一次运行里的临时文件名唯一。
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 文档里的一行：与前端 `DocLine`、任务书 v1.5/v1.6 的 serde 形状一一对应。
///
/// ```json
/// { "kind": "heading", "level": 1, "text": "上午" }
/// { "kind": "entry", "time": "08:47", "content": "…", "mood": "走神" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DocLine {
    /// 标题行：`#`…`######`，无时间戳、不能带心情。
    Heading { level: u8, text: String },
    /// 记录行：`HH:MM · 内容[ · 心情]`；`time: null` 表示由 Rust 回填当前时间。
    Entry {
        time: Option<String>,
        content: String,
        mood: Option<Mood>,
    },
}

/// `now` 所在的记录文件（06:00 跨天口径）。
fn data_file_for(app_paths: &AppPaths, now: DateTime<Local>) -> PathBuf {
    app_paths.data_file(&crate::day::day_key(now))
}

/// 解析标题行：0–3 个前导空格 + 1–6 个 `#` + 至少一个空白 + 非空文字。
fn parse_heading_line(line: &str) -> Option<DocLine> {
    let line = line.trim_end();
    let leading_spaces = line.len() - line.trim_start_matches(' ').len();
    if leading_spaces > 3 {
        return None;
    }

    let rest = &line[leading_spaces..];
    let hashes = rest.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }

    let after_hashes = &rest[hashes..];
    if !after_hashes.starts_with(' ') && !after_hashes.starts_with('\t') {
        return None;
    }

    let text = after_hashes.trim();
    if text.is_empty() {
        return None;
    }

    Some(DocLine::Heading {
        level: hashes as u8,
        text: text.to_string(),
    })
}

/// 读取并解析整篇文档。
///
/// - 文件不存在 → `Ok(vec![])`；
/// - 读取失败 → `Err(可读文案)` + ERROR 日志；
/// - 空行跳过；非法行跳过并写 WARN 日志（不阻塞其余行）。
fn read_document_from(path: &Path) -> Result<Vec<DocLine>, String> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            let message = format!("读取记录文档失败 {}: {error}", path.display());
            crate::log_error!("{message}");
            return Err(format!("读取今天的记录失败：{error}"));
        }
    };

    let mut lines = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        // 兼容带 UTF-8 BOM 的文件（例如用户用记事本另存过）。
        let line = if index == 0 {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };

        if line.trim().is_empty() {
            continue;
        }

        if let Some(heading) = parse_heading_line(line) {
            lines.push(heading);
            continue;
        }

        match record_line::parse_line(line) {
            Ok(entry) => lines.push(DocLine::Entry {
                time: Some(entry.time),
                content: entry.content,
                mood: entry.mood,
            }),
            Err(error) => {
                // 只记录行号与错误类型；parse_line 的错误不含正文。
                crate::log_warn!("跳过非法文档行（第 {} 行）：{error}", index + 1);
            }
        }
    }

    Ok(lines)
}

/// 是否严格 `HH:MM`（两位时分，0–23 / 0–59）。
fn is_valid_hhmm(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return false;
    }
    if !bytes[..2].iter().all(u8::is_ascii_digit) || !bytes[3..].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let hour = (bytes[0] - b'0') * 10 + (bytes[1] - b'0');
    let minute = (bytes[3] - b'0') * 10 + (bytes[4] - b'0');
    hour < 24 && minute < 60
}

/// 把任意一行文本压成单行（逐行 trim、换行折叠成一个空格）。
fn normalize_inline_text(text: &str) -> String {
    if !text.contains(['\r', '\n']) {
        return text.trim().to_string();
    }

    text.replace('\r', "\n")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// 规范化一次整篇保存：去空行、trim、校验级别/时间、回填缺失时间。
///
/// 所有 `time: null` 的记录行共用同一个 `now.format("%H:%M")`。
fn normalize_document(
    lines: Vec<DocLine>,
    now: DateTime<Local>,
) -> Result<Vec<DocLine>, String> {
    let fallback_time = now.format("%H:%M").to_string();
    let mut normalized = Vec::new();

    for line in lines {
        match line {
            DocLine::Heading { level, text } => {
                if !(1..=6).contains(&level) {
                    return Err(format!("标题级别必须是 1–6（收到 {level}）"));
                }
                let text = normalize_inline_text(&text);
                if text.is_empty() {
                    continue;
                }
                normalized.push(DocLine::Heading { level, text });
            }
            DocLine::Entry {
                time,
                content,
                mood,
            } => {
                let content = normalize_inline_text(&content);
                if content.is_empty() {
                    continue;
                }

                let time = match time {
                    Some(value) => {
                        let value = value.trim();
                        if !is_valid_hhmm(value) {
                            return Err(format!("记录时间必须是 HH:MM（收到 {value:?}）"));
                        }
                        value.to_string()
                    }
                    None => fallback_time.clone(),
                };

                normalized.push(DocLine::Entry {
                    time: Some(time),
                    content,
                    mood,
                });
            }
        }
    }

    Ok(normalized)
}

/// 把规范化后的文档格式化成文件内容（每行末尾 `\n`，无空行、无 BOM）。
fn format_document(lines: &[DocLine]) -> String {
    let mut output = String::new();
    for line in lines {
        match line {
            DocLine::Heading { level, text } => {
                output.push_str(&"#".repeat(usize::from(*level)));
                output.push(' ');
                output.push_str(text);
            }
            DocLine::Entry {
                time,
                content,
                mood,
            } => {
                let time = time.as_deref().unwrap_or_default();
                output.push_str(&record_line::format_line(time, content, *mood));
            }
        }
        output.push('\n');
    }
    output
}

/// 整篇写回前统计旧文件里的非法行：整篇保存会丢弃它们，需要 INFO 日志留痕。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExistingLineStats {
    /// 非空行总数
    total: usize,
    /// 既不是标题、也不是合法记录的行数
    invalid: usize,
}

fn analyze_existing_lines(raw: &str) -> ExistingLineStats {
    let mut stats = ExistingLineStats {
        total: 0,
        invalid: 0,
    };

    for (index, line) in raw.lines().enumerate() {
        let line = if index == 0 {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        if line.trim().is_empty() {
            continue;
        }
        stats.total += 1;
        if parse_heading_line(line).is_some() {
            continue;
        }
        if record_line::parse_line(line).is_ok() {
            continue;
        }
        stats.invalid += 1;
    }

    stats
}

/// 整篇保存成功后记录「丢弃旧文件非法行」的 INFO 日志（不写正文）。
fn log_dropped_invalid_lines(path: &Path, stats: Option<ExistingLineStats>) {
    if let Some(stats) = stats {
        if stats.invalid > 0 {
            crate::log_info!(
                "整篇保存：丢弃旧文件 {} 中无法解析的行 {} 行（旧文件非空行共 {} 行；不记录正文）",
                path.display(),
                stats.invalid,
                stats.total
            );
        }
    }
}

/// 原子写文件：同目录临时文件 → flush + sync → 替换目标。
///
/// 任一步失败都会删除临时文件并返回 `Err`；目标文件保持上一版内容
/// （不会出现只写一半的文档）。
fn write_document_atomic(path: &Path, content: &str) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| {
        let message = format!("创建记录文档目录失败 {}: {error}", parent.display());
        crate::log_error!("{message}");
        format!("保存记录失败：无法创建数据目录（{error}）")
    })?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("document.md");
    let sequence = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp_path = parent.join(format!(".{file_name}.tmp-{}-{sequence}", std::process::id()));

    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(content.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        // Windows 上 std::fs::rename 会替换已存在的目标文件。
        fs::rename(&temp_path, path)?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = fs::remove_file(&temp_path);
        let message = format!("原子写入记录文档失败 {}: {error}", path.display());
        crate::log_error!("{message}");
        return Err(format!("保存记录失败：无法写入记录文件（{error}）"));
    }

    Ok(())
}

/// 整篇写回（可注入路径与时间，供单测）。
fn save_document_to(
    path: &Path,
    now: DateTime<Local>,
    lines: Vec<DocLine>,
) -> Result<Vec<DocLine>, String> {
    let normalized = normalize_document(lines, now)?;

    let _guard = match WRITE_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            crate::log_warn!("记录写入锁已中毒，继续使用内部数据");
            poisoned.into_inner()
        }
    };

    // 写之前读旧文件统计非法行；写成功后再写 INFO 日志，失败时不误导。
    let existing_stats = fs::read_to_string(path)
        .ok()
        .map(|raw| analyze_existing_lines(&raw));

    if normalized.is_empty() {
        // 空文档不创建新文件；若原本有文件被清空，则原子写一个空文件
        // （保留「这天有文档」的状态，P2 读取 0 条记录会按既有逻辑跳过）。
        if path.exists() {
            write_document_atomic(path, "")?;
            log_dropped_invalid_lines(path, existing_stats);
        }
        return Ok(Vec::new());
    }

    let content = format_document(&normalized);
    write_document_atomic(path, &content)?;
    log_dropped_invalid_lines(path, existing_stats);

    let headings = normalized
        .iter()
        .filter(|line| matches!(line, DocLine::Heading { .. }))
        .count();
    crate::log_info!(
        "保存记录文档：{} 行（标题 {}，记录 {}）",
        normalized.len(),
        headings,
        normalized.len() - headings
    );

    Ok(normalized)
}

/// 读取当天文档（标题行 + 记录行，按文件顺序）。
#[tauri::command]
pub fn get_today_document() -> Result<Vec<DocLine>, String> {
    read_document_from(&data_file_for(paths::get(), Local::now()))
}

/// 保存目标：`None`/`"today"` = 今天，`"previous"` = 前一天（仅 06:00 跨天用）。
fn resolve_day_key(day: Option<&str>, now: DateTime<Local>) -> Result<String, String> {
    match day {
        None | Some("today") => Ok(crate::day::day_key(now)),
        Some("previous") => Ok(crate::day::previous_day_key(now)),
        Some(other) => Err(format!(
            "day 只接受 \"today\" 或 \"previous\"（收到 {other:?}）"
        )),
    }
}

/// 整篇写回当天文档；`time: null` 的记录行由 Rust 回填同一个当前 `HH:MM`。
///
/// `day = "previous"` 时写前一天文件：P0 的 `day-changed` 在 06:00 之后才到达，
/// 此时 `Local::now()` 已属于新一天，必须先按 `previous` 把旧一天的未落定内容写回，
/// 再加载新一天，避免写错天。
#[tauri::command]
pub fn save_today_document(
    lines: Vec<DocLine>,
    day: Option<String>,
) -> Result<Vec<DocLine>, String> {
    let now = Local::now();
    let target = day.as_deref();
    let day_key = resolve_day_key(target, now)?;
    let path = paths::get().data_file(&day_key);

    if target == Some("previous") {
        // 总控要求：跨天用 previous 保存时，日志记明日期、条数与原因。
        crate::log_info!(
            "跨天保存：day-changed 到达后先用 previous 日期 {day_key} 保存旧一天未落定内容（{} 行），避免写进新一天",
            lines.len()
        );
    }

    save_document_to(&path, now, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    use chrono::TimeZone;

    static TEMP_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    /// 测试用临时目录；`Drop` 时递归删除，绝不碰用户真实 `data/`。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let seq = TEMP_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "desktop-narrator-doc-{tag}-{}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("创建测试临时目录");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("本地时间应存在")
    }

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("创建测试数据目录");
        }
        fs::write(path, content).expect("写测试记录文件");
    }

    fn heading(level: u8, text: &str) -> DocLine {
        DocLine::Heading {
            level,
            text: text.to_string(),
        }
    }

    fn entry(time: Option<&str>, content: &str, mood: Option<Mood>) -> DocLine {
        DocLine::Entry {
            time: time.map(str::to_string),
            content: content.to_string(),
            mood,
        }
    }

    #[test]
    fn docline_serde_shape_matches_contract() {
        let heading = heading(2, "下午");
        let value = serde_json::to_value(&heading).expect("标题应能序列化");
        assert_eq!(
            value,
            serde_json::json!({ "kind": "heading", "level": 2, "text": "下午" })
        );

        let missing_time = entry(None, "写了周报", Some(Mood::Distracted));
        let value = serde_json::to_value(&missing_time).expect("记录应能序列化");
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "entry",
                "time": null,
                "content": "写了周报",
                "mood": "走神"
            })
        );

        let roundtrip: DocLine = serde_json::from_value(value).expect("应能反序列化");
        assert_eq!(roundtrip, missing_time);

        let no_mood = entry(Some("08:47"), "醒了", None);
        assert_eq!(
            serde_json::to_value(&no_mood).expect("应能序列化"),
            serde_json::json!({
                "kind": "entry",
                "time": "08:47",
                "content": "醒了",
                "mood": null
            })
        );
    }

    #[test]
    fn read_missing_document_is_empty() {
        let temp = TempDir::new("read-missing");
        let path = temp.path().join("2026-11-15.md");
        assert_eq!(
            read_document_from(&path).expect("缺失文件应视为空文档"),
            Vec::<DocLine>::new()
        );
    }

    #[test]
    fn read_document_parses_headings_and_entries_in_order() {
        let temp = TempDir::new("read-doc");
        let path = temp.path().join("2026-11-15.md");
        write_file(
            &path,
            "# 上午\n08:47 · 醒。看了眼时间，又睡了 · 想躲\n  ## 下午\n11:40 · 泡面，配一个讲时间管理的视频\n   ### 晚上\n#### 深夜\n",
        );

        let lines = read_document_from(&path).expect("应能读取文档");

        assert_eq!(
            lines,
            vec![
                heading(1, "上午"),
                entry(Some("08:47"), "醒。看了眼时间，又睡了", Some(Mood::Hiding)),
                heading(2, "下午"),
                entry(Some("11:40"), "泡面，配一个讲时间管理的视频", None),
                heading(3, "晚上"),
                heading(4, "深夜"),
            ]
        );
    }

    #[test]
    fn heading_recognition_supports_levels_one_to_six() {
        assert_eq!(parse_heading_line("# 上午"), Some(heading(1, "上午")));
        assert_eq!(parse_heading_line("## 下午"), Some(heading(2, "下午")));
        assert_eq!(parse_heading_line("   ### 晚上"), Some(heading(3, "晚上")));
        assert_eq!(parse_heading_line("#### 四级"), Some(heading(4, "四级")));
        assert_eq!(parse_heading_line("##### 五级"), Some(heading(5, "五级")));
        assert_eq!(parse_heading_line("###### 六级"), Some(heading(6, "六级")));
        // 前导空格超过 3 个不算标题
        assert_eq!(parse_heading_line("    # 上午"), None);
        // 没有空格不算标题
        assert_eq!(parse_heading_line("#上午"), None);
        // 只有 # 没有文字不算标题
        assert_eq!(parse_heading_line("# "), None);
        // 7 个 # 不算标题
        assert_eq!(parse_heading_line("####### 七级"), None);
    }

    #[test]
    fn heading_levels_four_to_six_roundtrip() {
        let temp = TempDir::new("deep-headings");
        let path = temp.path().join("2026-11-15.md");
        let original = vec![
            heading(4, "四级"),
            heading(5, "五级"),
            heading(6, "六级"),
        ];

        let saved = save_document_to(&path, at(2026, 11, 15, 8, 0), original.clone())
            .expect("应能保存");
        assert_eq!(saved, original);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "#### 四级\n##### 五级\n###### 六级\n"
        );
        assert_eq!(read_document_from(&path).expect("应能读取"), original);
    }

    #[test]
    fn save_document_writes_headings_and_entries() {
        let temp = TempDir::new("save-doc");
        let path = temp.path().join("2026-11-15.md");
        let now = at(2026, 11, 15, 8, 47);

        let saved = save_document_to(
            &path,
            now,
            vec![
                heading(1, "上午"),
                entry(Some("08:47"), "醒了", Some(Mood::Hiding)),
                entry(Some("11:40"), "泡面", None),
            ],
        )
        .expect("应能保存");

        assert_eq!(saved.len(), 3);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# 上午\n08:47 · 醒了 · 想躲\n11:40 · 泡面\n"
        );
    }

    #[test]
    fn save_document_backfills_missing_times_with_one_timestamp() {
        let temp = TempDir::new("backfill");
        let path = temp.path().join("2026-11-15.md");
        let now = at(2026, 11, 15, 9, 30);

        let saved = save_document_to(
            &path,
            now,
            vec![
                heading(2, "工作"),
                entry(None, "第一条", None),
                entry(None, "第二条", Some(Mood::Done)),
            ],
        )
        .expect("应能保存");

        assert_eq!(
            saved,
            vec![
                heading(2, "工作"),
                entry(Some("09:30"), "第一条", None),
                entry(Some("09:30"), "第二条", Some(Mood::Done)),
            ]
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "## 工作\n09:30 · 第一条\n09:30 · 第二条 · 做成了\n"
        );
    }

    #[test]
    fn save_document_preserves_existing_times() {
        let temp = TempDir::new("keep-times");
        let path = temp.path().join("2026-11-15.md");
        let saved = save_document_to(
            &path,
            at(2026, 11, 15, 23, 0),
            vec![entry(Some("08:47"), "旧记录", None), entry(None, "新记录", None)],
        )
        .expect("应能保存");

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "08:47 · 旧记录\n23:00 · 新记录\n"
        );
        assert_eq!(saved[0], entry(Some("08:47"), "旧记录", None));
        assert_eq!(saved[1], entry(Some("23:00"), "新记录", None));
    }

    #[test]
    fn save_document_skips_empty_content_and_normalizes_whitespace() {
        let temp = TempDir::new("skip-empty");
        let path = temp.path().join("2026-11-15.md");
        let saved = save_document_to(
            &path,
            at(2026, 11, 15, 8, 0),
            vec![
                heading(1, "   "),
                entry(None, "   ", None),
                entry(None, "  第一条  \n第二行  ", None),
                heading(2, "  上午  "),
            ],
        )
        .expect("应能保存");

        assert_eq!(
            saved,
            vec![
                entry(Some("08:00"), "第一条 第二行", None),
                heading(2, "上午"),
            ]
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "08:00 · 第一条 第二行\n## 上午\n"
        );
    }

    #[test]
    fn save_document_heading_leading_spaces_are_canonicalized() {
        let temp = TempDir::new("heading-canonical");
        let path = temp.path().join("2026-11-15.md");
        save_document_to(
            &path,
            at(2026, 11, 15, 8, 0),
            vec![heading(3, "晚上"), heading(1, "上午")],
        )
        .expect("应能保存");

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "### 晚上\n# 上午\n"
        );
    }

    #[test]
    fn save_empty_document_does_not_create_file() {
        let temp = TempDir::new("empty-no-file");
        let path = temp.path().join("2026-11-15.md");

        let saved = save_document_to(&path, at(2026, 11, 15, 8, 0), Vec::new())
            .expect("空文档应视为无操作");
        assert!(saved.is_empty());
        assert!(!path.exists(), "空文档不应创建文件");

        let saved = save_document_to(
            &path,
            at(2026, 11, 15, 8, 0),
            vec![entry(None, "   ", None), heading(1, "")],
        )
        .expect("全空行应视为无操作");
        assert!(saved.is_empty());
        assert!(!path.exists(), "全空内容不应创建文件");
    }

    #[test]
    fn save_empty_document_clears_existing_file() {
        let temp = TempDir::new("empty-existing");
        let path = temp.path().join("2026-11-15.md");
        write_file(&path, "08:00 · 旧记录\n");

        let saved = save_document_to(&path, at(2026, 11, 15, 9, 0), Vec::new())
            .expect("应能清空");

        assert!(saved.is_empty());
        assert!(path.exists(), "原本有文件时选择写空文件而不是删除");
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
        assert_eq!(
            read_document_from(&path).expect("空文件应读成空文档"),
            Vec::<DocLine>::new()
        );
    }

    #[test]
    fn save_document_preserves_order_and_supports_deletion() {
        let temp = TempDir::new("order-delete");
        let path = temp.path().join("2026-11-15.md");
        let first = vec![
            heading(1, "上午"),
            entry(Some("08:00"), "第一条", None),
            entry(Some("09:00"), "第二条", Some(Mood::Stuck)),
            entry(Some("10:00"), "第三条", None),
        ];
        save_document_to(&path, at(2026, 11, 15, 10, 0), first).expect("首次保存");

        let second = vec![
            heading(1, "上午"),
            entry(Some("08:00"), "第一条", None),
            entry(Some("10:00"), "第三条", None),
        ];
        let saved = save_document_to(&path, at(2026, 11, 15, 10, 30), second).expect("删行后保存");

        assert_eq!(saved.len(), 3);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# 上午\n08:00 · 第一条\n10:00 · 第三条\n"
        );
    }

    #[test]
    fn save_document_replaces_existing_file_atomically() {
        let temp = TempDir::new("atomic");
        let path = temp.path().join("2026-11-15.md");
        save_document_to(
            &path,
            at(2026, 11, 15, 8, 0),
            vec![entry(Some("08:00"), "旧内容", None)],
        )
        .expect("首次保存");
        save_document_to(
            &path,
            at(2026, 11, 15, 9, 0),
            vec![entry(Some("09:00"), "新内容", None)],
        )
        .expect("替换保存");

        assert_eq!(fs::read_to_string(&path).unwrap(), "09:00 · 新内容\n");
        let leftovers: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "不应留下临时文件: {leftovers:?}");
    }

    #[test]
    fn save_document_rejects_invalid_heading_level_without_touching_file() {
        let temp = TempDir::new("invalid-level");
        let path = temp.path().join("2026-11-15.md");
        save_document_to(
            &path,
            at(2026, 11, 15, 8, 0),
            vec![entry(Some("08:00"), "旧内容", None)],
        )
        .expect("首次保存");
        let before = fs::read_to_string(&path).unwrap();

        let error = save_document_to(
            &path,
            at(2026, 11, 15, 9, 0),
            vec![heading(7, "七级"), entry(None, "新内容", None)],
        )
        .expect_err("非法标题级别应被拒绝");
        assert!(error.contains("标题级别"), "实际错误: {error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), before, "失败后文件必须保持上一版");
    }

    #[test]
    fn save_document_rejects_invalid_time_without_touching_file() {
        let temp = TempDir::new("invalid-time");
        let path = temp.path().join("2026-11-15.md");
        write_file(&path, "08:00 · 旧内容\n");

        for bad_time in ["25:00", "12:60", "8:00", "abc"] {
            let error = save_document_to(
                &path,
                at(2026, 11, 15, 9, 0),
                vec![entry(Some(bad_time), "新内容", None)],
            )
            .expect_err("非法时间应被拒绝");
            assert!(error.contains("HH:MM"), "实际错误: {error}");
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                "08:00 · 旧内容\n",
                "失败后文件必须保持上一版"
            );
        }
    }

    #[test]
    fn save_document_write_failure_keeps_directory_and_leaves_no_temp_file() {
        let temp = TempDir::new("write-failure");
        let path = temp.path().join("2026-11-15.md");
        fs::create_dir_all(&path).expect("用目录占住目标路径");

        let error = save_document_to(
            &path,
            at(2026, 11, 15, 8, 0),
            vec![entry(None, "内容", None)],
        )
        .expect_err("目标路径是目录时应失败");
        assert!(error.contains("保存记录失败"), "实际错误: {error}");
        assert!(path.is_dir(), "失败后原路径状态不应被改变");

        let leftovers: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "失败后不应留下临时文件: {leftovers:?}");
    }

    #[test]
    fn entry_content_with_separator_roundtrips() {
        let temp = TempDir::new("separator");
        let path = temp.path().join("2026-11-15.md");
        let saved = save_document_to(
            &path,
            at(2026, 11, 15, 10, 0),
            vec![heading(2, "工作"), entry(None, "写周报 · 改了三版", Some(Mood::Distracted))],
        )
        .expect("应能保存");

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "## 工作\n10:00 · 写周报 · 改了三版 · 走神\n"
        );
        let read_back = read_document_from(&path).expect("应能读取");
        assert_eq!(read_back, saved);
        assert_eq!(read_back[1], entry(Some("10:00"), "写周报 · 改了三版", Some(Mood::Distracted)));
    }

    #[test]
    fn read_document_skips_invalid_lines_but_keeps_rest() {
        let temp = TempDir::new("invalid-lines");
        let path = temp.path().join("2026-11-15.md");
        write_file(
            &path,
            "这不是记录\n# 上午\n25:00 · 坏时间\n08:00 · 好的\n\n#### 四级\n09:00 · 带心情 · 走神\n",
        );

        let lines = read_document_from(&path).expect("非法行跳过，其余应成功");

        assert_eq!(
            lines,
            vec![
                heading(1, "上午"),
                entry(Some("08:00"), "好的", None),
                heading(4, "四级"),
                entry(Some("09:00"), "带心情", Some(Mood::Distracted)),
            ]
        );
    }

    #[test]
    fn analyze_existing_lines_counts_only_invalid_non_empty_lines() {
        let stats = analyze_existing_lines(
            "# 上午\n08:00 · 好的\n坏行\n\n25:00 · 坏时间\n####### 七级\n",
        );
        assert_eq!(stats.total, 5);
        assert_eq!(stats.invalid, 3);
    }

    #[test]
    fn save_document_drops_invalid_old_lines() {
        let temp = TempDir::new("drop-invalid");
        let path = temp.path().join("2026-11-15.md");
        write_file(&path, "# 上午\n坏行\n08:00 · 好的\n");

        let saved = save_document_to(
            &path,
            at(2026, 11, 15, 9, 0),
            vec![heading(1, "上午"), entry(Some("08:00"), "好的", None)],
        )
        .expect("应能保存");

        assert_eq!(saved.len(), 2);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# 上午\n08:00 · 好的\n"
        );
    }

    #[test]
    fn read_document_tolerates_utf8_bom() {
        let temp = TempDir::new("bom");
        let path = temp.path().join("2026-11-15.md");
        write_file(&path, "\u{feff}# 上午\n08:00 · 带 BOM 的第一行\n");

        let lines = read_document_from(&path).expect("应忽略 BOM");
        assert_eq!(
            lines,
            vec![heading(1, "上午"), entry(Some("08:00"), "带 BOM 的第一行", None)]
        );
    }

    #[test]
    fn document_roundtrip_preserves_titles_entries_moods_and_order() {
        let temp = TempDir::new("roundtrip");
        let path = temp.path().join("2026-11-15.md");
        let original = vec![
            heading(1, "上午"),
            entry(Some("08:47"), "醒。看了眼时间，又睡了", Some(Mood::Hiding)),
            heading(2, "工作"),
            entry(Some("10:00"), "写周报 · 改了三版", Some(Mood::Distracted)),
            heading(3, "晚上"),
            entry(Some("20:00"), "泡面", None),
        ];

        save_document_to(&path, at(2026, 11, 15, 21, 0), original.clone()).expect("保存");
        let read_back = read_document_from(&path).expect("读取");

        assert_eq!(read_back, original);
    }

    #[test]
    fn resolve_day_key_supports_today_and_previous() {
        let now = at(2026, 11, 16, 6, 0);
        assert_eq!(resolve_day_key(None, now).unwrap(), "2026-11-16");
        assert_eq!(resolve_day_key(Some("today"), now).unwrap(), "2026-11-16");
        // 06:00 之后 day-changed 到达时，previous 指向刚结束的那一天。
        assert_eq!(resolve_day_key(Some("previous"), now).unwrap(), "2026-11-15");

        let later = at(2026, 11, 16, 7, 0);
        assert_eq!(resolve_day_key(Some("previous"), later).unwrap(), "2026-11-15");

        assert!(resolve_day_key(Some("yesterday"), later).is_err());
    }

    #[test]
    fn heading_only_document_roundtrips() {
        let temp = TempDir::new("heading-only");
        let path = temp.path().join("2026-11-15.md");
        let original = vec![heading(1, "上午"), heading(2, "下午"), heading(3, "晚上")];

        let saved = save_document_to(&path, at(2026, 11, 15, 8, 0), original.clone())
            .expect("保存");
        assert_eq!(saved, original);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# 上午\n## 下午\n### 晚上\n"
        );
    }
}
