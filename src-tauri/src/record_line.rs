//! 记录行格式：`HH:MM · 内容 · 心情`（心情可省略；心情为任意已配置词）。
//!
//! 解析规则（策划案 4.2 / 8 + 任务书 R14）：按 ` · ` 分段；首段必须是合法 `HH:MM`；
//! 末段 trim 后若命中「已知心情词」（默认六词 ∪ 用户自定义词，含被隐藏的）则视为心情；
//! 其余段用 ` · ` 拼回内容。因此「内容里含 ` · `」的记录可以原样往返。
//!
//! 兼容性（R14，重要）：`Mood` 由枚举改为可承载任意心情字符串的 newtype，serde 仍是字符串；
//! 保留 `Mood::ALL` / `Mood::as_str()` / `Mood::from_word()` 与旧枚举同名常量
//! （`Mood::Done`…`Mood::Hiding`），并保持 `Copy`，所以 P1/P2 现有调用不需要改动。

use std::collections::HashSet;
use std::fmt;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::moods::DEFAULT_MOODS;

/// 行内分隔符：空格 + 中点 + 空格。
pub const SEPARATOR: &str = " · ";

/// 六动词（固定顺序），同时用于展示与解析；与 `moods::DEFAULT_MOODS` 同一来源。
pub const MOOD_WORDS: [&str; 6] = DEFAULT_MOODS;

/// 心情：承载任意心情词的 newtype（默认六词或用户自定义词），serde 序列化为字符串。
///
/// 内部用进程级驻留池（[`intern_mood_word`]）保存 `&'static str`，因此仍是 `Copy`，
/// 且保留了旧枚举的 `Mood::Done`…`Mood::Hiding` 常量；P1/P2 现有代码无需改动。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Mood(&'static str);

// R14 兼容层：`ALL` 与 `Done`…`Hiding` 是旧枚举 API 的等价常量，当前只有测试/P1 使用；
// 保留它们是为了不打断 P1/P2 既有代码，因此允许 dead_code（逻辑上属于对外兼容面）。
#[allow(dead_code)]
impl Mood {
    /// 默认六词（固定顺序）。
    pub const ALL: [Mood; 6] = [
        Mood::Done,
        Mood::Stuck,
        Mood::Enduring,
        Mood::Distracted,
        Mood::Idling,
        Mood::Hiding,
    ];

    // 旧枚举变体的等价常量：仅为兼容 P1 `records.rs` / P2 的既有调用而保留。
    #[allow(non_upper_case_globals)]
    pub const Done: Mood = Mood("做成了");
    #[allow(non_upper_case_globals)]
    pub const Stuck: Mood = Mood("卡住");
    #[allow(non_upper_case_globals)]
    pub const Enduring: Mood = Mood("硬撑");
    #[allow(non_upper_case_globals)]
    pub const Distracted: Mood = Mood("走神");
    #[allow(non_upper_case_globals)]
    pub const Idling: Mood = Mood("空转");
    #[allow(non_upper_case_globals)]
    pub const Hiding: Mood = Mood("想躲");

    /// 心情词字面量。
    pub fn as_str(self) -> &'static str {
        self.0
    }

    /// 从「已知心情词」（默认 ∪ 自定义，含隐藏词）构造；不是已知词时返回 `None`。
    pub fn from_word(word: &str) -> Option<Self> {
        let word = word.trim();
        // 默认六词走常量快路径；自定义词（含 hidden）查全局 known 集合。
        if MOOD_WORDS.contains(&word) || crate::moods::known_contains(word) {
            Mood::custom(word)
        } else {
            None
        }
    }

    /// 新增自定义词的构造入口：任意合法心情词（trim 后 1–8 字、不含 ` · `/换行/制表符）。
    ///
    /// 已存在的词会复用驻留池里的同一份字符串；非法词返回 `None`。
    pub fn custom(word: &str) -> Option<Self> {
        let word = crate::moods::validate_mood_word(word).ok()?;
        Some(Mood(intern_mood_word(word)))
    }
}

/// 进程级心情词驻留池：同一字符串只分配一次并常驻，返回 `&'static str`。
///
/// 词表规模由用户心情配置决定（默认六词 + 自定义 + 历史词，数量有限），
/// 不随解析/保存次数增长；`Mood` 因此可以保持 `Copy`，与旧枚举调用方式兼容。
pub(crate) fn intern_mood_word(word: &str) -> &'static str {
    static POOL: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let pool = POOL.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = pool
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = guard.get(word) {
        return *existing;
    }
    let leaked: &'static str = Box::leak(word.to_string().into_boxed_str());
    guard.insert(leaked);
    leaked
}

impl fmt::Display for Mood {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for Mood {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Mood {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let word = String::deserialize(deserializer)?;
        let word = crate::moods::validate_mood_word(&word)
            .map_err(|error| serde::de::Error::custom(error))?;
        Ok(Mood(intern_mood_word(word)))
    }
}

/// 一条记录；与前端 `src/types.ts` 的 `Entry` 一一对应（`mood` 序列化为字符串或 null）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// 本地时间 HH:MM
    pub time: String,
    pub content: String,
    /// 可省略的心情；任意已配置心情词
    pub mood: Option<Mood>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    EmptyLine,
    InvalidTime(String),
    MissingContent,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::EmptyLine => formatter.write_str("空行"),
            ParseError::InvalidTime(value) => {
                write!(formatter, "非法时间戳（应为 HH:MM）: {value:?}")
            }
            ParseError::MissingContent => formatter.write_str("缺少记录内容"),
        }
    }
}

impl std::error::Error for ParseError {}

/// 把一条记录格式化为文件里的一行。
pub fn format_line(time: &str, content: &str, mood: Option<Mood>) -> String {
    match mood {
        Some(mood) => format!("{time}{SEPARATOR}{content}{SEPARATOR}{mood}"),
        None => format!("{time}{SEPARATOR}{content}"),
    }
}

/// 解析文件里的一行；非法行返回错误而不是 panic。
///
/// 心情判定使用全局 known 集合（默认 ∪ 自定义，含隐藏词，由 `moods` 模块维护）。
pub fn parse_line(line: &str) -> Result<Entry, ParseError> {
    parse_line_with(line, |word| Mood::from_word(word).is_some())
}

/// 与 [`parse_line`] 相同，但心情判定改用注入的谓词（供测试/特殊场景）。
///
/// 例：`parse_line_with(line, |word| known.contains(&word))`。
pub fn parse_line_with<F>(line: &str, is_known_mood: F) -> Result<Entry, ParseError>
where
    F: Fn(&str) -> bool,
{
    let line = line.trim();
    if line.is_empty() {
        return Err(ParseError::EmptyLine);
    }

    let mut segments = line.split(SEPARATOR);
    let time = segments.next().unwrap_or_default().trim();
    if !is_valid_hhmm(time) {
        return Err(ParseError::InvalidTime(time.to_string()));
    }

    let rest: Vec<&str> = segments.collect();
    let mut mood = None;
    let mut content_segments: &[&str] = &rest;

    if let Some(last) = rest.last() {
        // 末段必须「整段」命中已知心情词才算心情，否则归内容。
        let candidate = last.trim();
        if !candidate.is_empty() && is_known_mood(candidate) {
            if let Some(parsed) = Mood::custom(candidate) {
                mood = Some(parsed);
                content_segments = &rest[..rest.len() - 1];
            }
        }
    }

    let content = content_segments.join(SEPARATOR).trim().to_string();
    if content.is_empty() {
        return Err(ParseError::MissingContent);
    }

    Ok(Entry {
        time: time.to_string(),
        content,
        mood,
    })
}

/// 严格 `HH:MM`（两位小时、两位分钟，0–23 / 0–59）。
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用 known 集合：默认六词 + 一个自定义词「摸鱼」。
    const TEST_KNOWN: [&str; 7] = ["做成了", "卡住", "硬撑", "走神", "空转", "想躲", "摸鱼"];

    fn is_test_known(word: &str) -> bool {
        TEST_KNOWN.contains(&word)
    }

    #[test]
    fn roundtrip_with_each_mood() {
        let content = "醒。看了眼时间，又睡了";
        for mood in Mood::ALL {
            let line = format_line("08:47", content, Some(mood));
            let entry = parse_line(&line).expect("应能解析");
            assert_eq!(
                entry,
                Entry {
                    time: "08:47".to_string(),
                    content: content.to_string(),
                    mood: Some(mood),
                }
            );
        }
    }

    #[test]
    fn roundtrip_without_mood() {
        let line = format_line("11:40", "泡面，配一个讲时间管理的视频", None);
        assert_eq!(line, "11:40 · 泡面，配一个讲时间管理的视频");
        let entry = parse_line(&line).expect("应能解析");
        assert_eq!(entry.time, "11:40");
        assert_eq!(entry.content, "泡面，配一个讲时间管理的视频");
        assert_eq!(entry.mood, None);
    }

    #[test]
    fn content_containing_separator_roundtrips() {
        let content = "泡面 · 配一个讲时间管理的视频";
        let line = format_line("11:40", content, None);
        let entry = parse_line(&line).expect("应能解析");
        assert_eq!(entry.content, content);
        assert_eq!(entry.mood, None);

        // 内容里出现六动词语素、但末段不是「整段心情词」时，整段都是内容
        let content_with_mood_word = "跟他说 · 走神了，然后继续干活";
        let line = format!("09:00{SEPARATOR}{content_with_mood_word}");
        let entry = parse_line(&line).expect("应能解析");
        assert_eq!(entry.content, content_with_mood_word);
        assert_eq!(entry.mood, None);
    }

    #[test]
    fn content_ending_with_mood_word_is_parsed_as_mood() {
        // 契约规定：末段整段命中已知心情词即视为心情。
        // 因此内容本身以「 · 走神」结尾时无法与心情区分，这是既定的行格式语义。
        let line = "09:00 · 跟他说 · 走神";
        let entry = parse_line(line).expect("应能解析");
        assert_eq!(entry.content, "跟他说");
        assert_eq!(entry.mood, Some(Mood::Distracted));
    }

    #[test]
    fn separator_mood_is_detected_from_last_segment() {
        let line = "16:30 · 便利店，买了冰咖啡，把饭团放回去了 · 走神";
        let entry = parse_line(line).expect("应能解析");
        assert_eq!(entry.time, "16:30");
        assert_eq!(entry.content, "便利店，买了冰咖啡，把饭团放回去了");
        assert_eq!(entry.mood, Some(Mood::Distracted));
    }

    #[test]
    fn invalid_lines_return_errors() {
        assert_eq!(parse_line(""), Err(ParseError::EmptyLine));
        assert_eq!(parse_line("   "), Err(ParseError::EmptyLine));

        assert!(matches!(parse_line("25:00 · x"), Err(ParseError::InvalidTime(_))));
        assert!(matches!(parse_line("12:60 · x"), Err(ParseError::InvalidTime(_))));
        assert!(matches!(parse_line("8:47 · x"), Err(ParseError::InvalidTime(_))));
        assert!(matches!(parse_line("abc · x"), Err(ParseError::InvalidTime(_))));

        // 只有时间没有内容
        assert!(matches!(parse_line("08:47"), Err(ParseError::MissingContent)));
        // 只有时间 + 心情，内容为空
        assert!(matches!(
            parse_line("08:47 · 走神"),
            Err(ParseError::MissingContent)
        ));
    }

    #[test]
    fn trims_outer_whitespace() {
        let entry = parse_line("  08:00 · 醒了  ").expect("应能解析");
        assert_eq!(entry.time, "08:00");
        assert_eq!(entry.content, "醒了");
    }

    #[test]
    fn mood_words_are_the_six_fixed_verbs() {
        assert_eq!(MOOD_WORDS.len(), 6);
        for (index, mood) in Mood::ALL.into_iter().enumerate() {
            assert_eq!(mood.as_str(), MOOD_WORDS[index]);
            assert_eq!(Mood::from_word(mood.as_str()), Some(mood));
        }
        assert_eq!(Mood::from_word("不存在"), None);
    }

    // ---- R14 自定义心情 ----

    #[test]
    fn custom_mood_roundtrips_with_injected_known() {
        let mood = Mood::custom("摸鱼").expect("自定义心情应可构造");
        let line = format_line("10:00", "写周报 · 改了三版", Some(mood));
        assert_eq!(line, "10:00 · 写周报 · 改了三版 · 摸鱼");

        let entry = parse_line_with(&line, is_test_known).expect("应能解析");
        assert_eq!(
            entry,
            Entry {
                time: "10:00".to_string(),
                content: "写周报 · 改了三版".to_string(),
                mood: Some(mood),
            }
        );
        assert_eq!(entry.mood.map(Mood::as_str), Some("摸鱼"));
    }

    #[test]
    fn custom_known_word_is_not_parsed_without_known_injection() {
        // 测试进程未加载用户配置，全局 known 只有默认六词 →「摸鱼」归内容。
        let entry = parse_line("10:00 · 写周报 · 摸鱼").expect("应能解析");
        assert_eq!(entry.mood, None);
        assert_eq!(entry.content, "写周报 · 摸鱼");
    }

    #[test]
    fn mood_serde_serializes_as_plain_string() {
        let custom = Mood::custom("摸鱼").expect("自定义心情应可构造");
        assert_eq!(
            serde_json::to_value(custom).expect("序列化心情"),
            serde_json::json!("摸鱼")
        );
        assert_eq!(
            serde_json::to_value(Mood::Hiding).expect("序列化默认心情"),
            serde_json::json!("想躲")
        );

        let parsed: Mood = serde_json::from_value(serde_json::json!("摸鱼")).expect("反序列化自定义心情");
        assert_eq!(parsed, custom);
        let parsed_default: Mood =
            serde_json::from_value(serde_json::json!("走神")).expect("反序列化默认心情");
        assert_eq!(parsed_default, Mood::Distracted);

        assert!(serde_json::from_value::<Mood>(serde_json::json!("")).is_err());
        assert!(serde_json::from_value::<Mood>(serde_json::json!("摸 · 鱼")).is_err());
    }

    #[test]
    fn custom_mood_constructor_rejects_invalid_words() {
        assert!(Mood::custom("").is_none());
        assert!(Mood::custom("   ").is_none());
        assert_eq!(Mood::custom(" 摸鱼 ").map(Mood::as_str), Some("摸鱼"));
        assert!(Mood::custom("一二三四五六七八九").is_none());
        assert!(Mood::custom("摸 · 鱼").is_none());
        assert!(Mood::custom("摸\n鱼").is_none());
        assert!(Mood::custom("摸\t鱼").is_none());
    }
}
