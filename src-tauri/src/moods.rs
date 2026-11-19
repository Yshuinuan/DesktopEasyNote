//! 自定义心情系统数据层（任务书 R14）。
//!
//! 默认六词之外，用户可以添加自定义心情；默认词也可以删除——**删除 = 仅隐藏**，
//! 历史记录里的该词仍然属于 known 集合，所以仍会被解析为心情而不是正文。
//!
//! - `known`   = 默认 ∪ custom（**包含 hidden**，保证历史记录可识别）
//! - `visible` = (默认 ∪ custom) − hidden；顺序：默认在前，自定义按添加顺序
//! - 启动时从 `config.json` 加载一次并缓存；增删后写回配置并更新内存；`RwLock` 保证线程安全
//! - 写回时总是先完整重读 `config.json` 再写，绝不丢失 `api_key`/`base_url`/`model`
//! - 本模块不读取、不打印 apikey 明文

use std::path::Path;
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::config::{Config, MoodsConfig};
use crate::record_line::{intern_mood_word, SEPARATOR};

/// 默认六词（固定顺序，展示与解析顺序）。
pub const DEFAULT_MOODS: [&str; 6] = ["做成了", "卡住", "硬撑", "走神", "空转", "想躲"];

/// 单个心情词的 Unicode 字符数上限。
pub const MAX_MOOD_CHARS: usize = 8;

/// 可见心情总数上限（默认 + 自定义，去掉 hidden 之后）。
pub const MAX_VISIBLE_MOODS: usize = 20;

/// 给前端 / IPC 命令的心情视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoodsView {
    /// 菜单可见顺序：默认词在前（跳过 hidden），自定义词按添加顺序。
    pub visible: Vec<String>,
    /// 默认六词（固定顺序；含被隐藏的，便于前端识别默认词）。
    pub defaults: Vec<String>,
    /// 用户自定义词（按添加顺序；含被隐藏的）。
    pub custom: Vec<String>,
    /// 被删除（仅隐藏）的词，可能含默认词。
    pub hidden: Vec<String>,
}

/// 校验并规范化一个心情词（trim 后）：非空、≤8 个 Unicode 字符、不含 ` · `/换行/制表符。
///
/// 返回 trim 后的词；失败返回可读字符串（IPC 直接展示）。
pub(crate) fn validate_mood_word(word: &str) -> Result<&str, String> {
    let word = word.trim();
    if word.is_empty() {
        return Err("心情不能为空".to_string());
    }
    if word.chars().count() > MAX_MOOD_CHARS {
        return Err(format!("心情最多 {MAX_MOOD_CHARS} 个字"));
    }
    if word.contains(SEPARATOR) {
        return Err(format!("心情不能包含分隔符「{SEPARATOR}」"));
    }
    if word.contains(['\n', '\r', '\t']) {
        return Err("心情不能包含换行或制表符".to_string());
    }
    Ok(word)
}

/// 在默认六词里查一个词，返回 `'static` 字面量（避免重复驻留）。
fn default_word(word: &str) -> Option<&'static str> {
    DEFAULT_MOODS
        .iter()
        .copied()
        .find(|candidate| *candidate == word)
}

/// 内存中的心情状态（词都驻留为 `&'static str`，见 `record_line::intern_mood_word`）。
#[derive(Debug, Clone, Default)]
struct State {
    /// 用户自定义词，按添加顺序。
    custom: Vec<&'static str>,
    /// 被删除（仅隐藏）的词，可能是默认词或自定义词。
    hidden: Vec<&'static str>,
}

impl State {
    /// 从配置构建：清理非法/重复/已知默认词条目；手改配置超上限时按顺序截断。
    fn from_config(config: &Config) -> Self {
        let mut state = Self::default();
        let Some(moods) = config.moods.as_ref() else {
            return state;
        };

        for raw in &moods.custom {
            let Ok(word) = validate_mood_word(raw) else {
                continue;
            };
            // 默认词不需要再进 custom
            if default_word(word).is_some() {
                continue;
            }
            if state.custom.iter().any(|known| *known == word) {
                continue;
            }
            state.custom.push(intern_mood_word(word));
        }

        for raw in &moods.hidden {
            let Ok(word) = validate_mood_word(raw) else {
                continue;
            };
            // hidden 里只保留 known 词：默认词，或已存在的自定义词。
            let interned = match default_word(word) {
                Some(default) => default,
                None => match state.custom.iter().copied().find(|known| *known == word) {
                    Some(custom) => custom,
                    None => continue,
                },
            };
            if state.hidden.iter().any(|hidden| *hidden == word) {
                continue;
            }
            state.hidden.push(interned);
        }

        // 兼容手改配置：可见数超上限时从末尾移除未隐藏的自定义词。
        while state.visible_count() > MAX_VISIBLE_MOODS {
            match state
                .custom
                .iter()
                .rposition(|word| !state.is_hidden(word))
            {
                Some(index) => {
                    state.custom.remove(index);
                }
                None => break,
            }
        }

        state
    }

    fn is_hidden(&self, word: &str) -> bool {
        self.hidden.iter().any(|hidden| *hidden == word)
    }

    fn visible(&self) -> Vec<String> {
        let mut words: Vec<String> = Vec::new();
        for word in DEFAULT_MOODS {
            if !self.is_hidden(word) {
                words.push(word.to_string());
            }
        }
        for word in &self.custom {
            if !self.is_hidden(word) {
                words.push((*word).to_string());
            }
        }
        words
    }

    fn visible_count(&self) -> usize {
        DEFAULT_MOODS
            .iter()
            .filter(|word| !self.is_hidden(word))
            .count()
            + self
                .custom
                .iter()
                .filter(|word| !self.is_hidden(word))
                .count()
    }

    /// known = 默认 ∪ custom（含 hidden 词）。
    fn known_contains(&self, word: &str) -> bool {
        default_word(word).is_some() || self.custom.iter().any(|known| *known == word)
    }

    #[cfg(test)]
    fn known_words(&self) -> Vec<&'static str> {
        DEFAULT_MOODS
            .iter()
            .copied()
            .chain(self.custom.iter().copied())
            .collect()
    }

    fn view(&self) -> MoodsView {
        MoodsView {
            visible: self.visible(),
            defaults: DEFAULT_MOODS.iter().map(|word| word.to_string()).collect(),
            custom: self.custom.iter().map(|word| word.to_string()).collect(),
            hidden: self.hidden.iter().map(|word| word.to_string()).collect(),
        }
    }

    /// 添加 / 恢复（`word` 已由 `validate_mood_word` 规范化）。
    fn add(&mut self, word: &str) -> Result<(), String> {
        // 命中 hidden → 恢复（默认词恢复后仍是默认词，不进 custom）；恢复同样受可见上限约束
        if self.is_hidden(word) {
            if self.visible_count() >= MAX_VISIBLE_MOODS {
                return Err(format!(
                    "心情最多 {MAX_VISIBLE_MOODS} 个，请先删除一个再恢复"
                ));
            }
            self.hidden.retain(|hidden| *hidden != word);
            return Ok(());
        }
        if self.known_contains(word) {
            return Err(format!("心情「{word}」已存在"));
        }
        if self.visible_count() >= MAX_VISIBLE_MOODS {
            return Err(format!("心情最多 {MAX_VISIBLE_MOODS} 个，请先删除一个再添加"));
        }
        self.custom.push(intern_mood_word(word));
        Ok(())
    }

    /// 删除 = 仅加入 hidden；不删除 custom，默认词同样适用。
    fn delete(&mut self, word: &str) -> Result<(), String> {
        if !self.visible().iter().any(|visible| visible == word) {
            return Err(format!("心情「{word}」不在可见列表中"));
        }
        self.hidden.push(intern_mood_word(word));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 全局缓存（启动加载一次；增删后写回并更新）
// ---------------------------------------------------------------------------

static STATE: OnceLock<RwLock<State>> = OnceLock::new();

fn state() -> &'static RwLock<State> {
    STATE.get_or_init(|| RwLock::new(State::default()))
}

fn read_state<R>(read: impl FnOnce(&State) -> R) -> R {
    let guard = state()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    read(&guard)
}

fn write_state<R>(write: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = state()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    write(&mut guard)
}

/// 启动时调用一次（`lib.rs` setup）：从已加载的配置建立缓存。
///
/// 不读取 key；只记可见/自定义/隐藏数量。
pub fn init(config: &Config) {
    let loaded = State::from_config(config);
    let view = loaded.view();
    write_state(|state| *state = loaded);
    crate::log_info!(
        "心情配置加载完成（可见 {} 个：默认 {}，自定义 {}；隐藏 {} 个）",
        view.visible.len(),
        view.defaults.len(),
        view.custom.len(),
        view.hidden.len()
    );
}

/// 当前心情视图。
pub fn view() -> MoodsView {
    read_state(State::view)
}

/// known = 默认 ∪ custom（**含 hidden**），历史记录能识别为心情的依据。
///
/// R14 数据层语义入口：需要完整集合的调用方/测试用；解析热路径用 [`known_contains`]。
#[allow(dead_code)]
pub fn known() -> Vec<String> {
    read_state(|state| {
        DEFAULT_MOODS
            .iter()
            .copied()
            .chain(state.custom.iter().copied())
            .map(str::to_string)
            .collect()
    })
}

/// visible = (默认 ∪ custom) − hidden；默认在前、自定义按添加顺序。
#[allow(dead_code)]
pub fn visible() -> Vec<String> {
    read_state(State::visible)
}

/// `word` 是否属于 known 集合（默认 ∪ 自定义，**含 hidden**）。`parse_line` 用它判定心情。
pub fn known_contains(word: &str) -> bool {
    let word = word.trim();
    read_state(|state| state.known_contains(word))
}

/// 添加心情词；命中 hidden 视为恢复。成功返回新视图，失败返回可读字符串。
pub fn add(word: &str) -> Result<MoodsView, String> {
    let config_file = crate::paths::get().config_file.clone();
    let view = write_state(|state| add_at(&config_file, state, word))?;
    crate::log_info!(
        "心情：添加/恢复「{}」（当前可见 {} 个）",
        word.trim(),
        view.visible.len()
    );
    Ok(view)
}

/// 删除心情词（仅隐藏，不删 custom；默认词同样适用）。成功返回新视图。
pub fn delete(word: &str) -> Result<MoodsView, String> {
    let config_file = crate::paths::get().config_file.clone();
    let view = write_state(|state| delete_at(&config_file, state, word))?;
    crate::log_info!(
        "心情：已隐藏「{}」（当前可见 {} 个）",
        word.trim(),
        view.visible.len()
    );
    Ok(view)
}

// ---------------------------------------------------------------------------
// 可注入配置路径的内部实现（单测用临时 config，不碰真实配置）
// ---------------------------------------------------------------------------

fn add_at(config_file: &Path, state: &mut State, word: &str) -> Result<MoodsView, String> {
    let word = validate_mood_word(word)?;
    let mut candidate = state.clone();
    candidate.add(word)?;
    persist_to(config_file, &candidate)?;
    *state = candidate;
    Ok(state.view())
}

fn delete_at(config_file: &Path, state: &mut State, word: &str) -> Result<MoodsView, String> {
    let word = validate_mood_word(word)?;
    let mut candidate = state.clone();
    candidate.delete(word)?;
    persist_to(config_file, &candidate)?;
    *state = candidate;
    Ok(state.view())
}

/// 写回配置：先完整重读（保留 api_key/base_url/model），只替换 `moods` 字段。
fn persist_to(config_file: &Path, state: &State) -> Result<(), String> {
    let mut config = Config::load_from(config_file);
    config.moods = Some(MoodsConfig {
        custom: state.custom.iter().map(|word| word.to_string()).collect(),
        hidden: state.hidden.iter().map(|word| word.to_string()).collect(),
    });
    config.save_to(config_file)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::record_line::{format_line, parse_line_with, Mood};

    /// 假 key：仅用于验证写回不丢字段，不是真实 key。
    const PLACEHOLDER_KEY: &str = "placeholder-not-a-real-key";

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "desktop-narrator-moods-{tag}-{}-{seq}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("创建测试临时目录");
            Self { path }
        }

        fn config_file(&self) -> PathBuf {
            self.path.join("config.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn sample_config(moods: Option<MoodsConfig>) -> Config {
        Config {
            api_key: Some(PLACEHOLDER_KEY.to_string()),
            base_url: "https://api.deepseek.com".to_string(),
            model: "deepseek-chat".to_string(),
            moods,
            heading_presets: None,
        }
    }

    fn write_config(path: &Path, config: &Config) {
        config.save_to(path).expect("写入测试配置");
    }

    fn load_state(path: &Path) -> State {
        State::from_config(&Config::load_from(path))
    }

    fn moods_config(custom: &[&str], hidden: &[&str]) -> MoodsConfig {
        MoodsConfig {
            custom: custom.iter().map(|word| word.to_string()).collect(),
            hidden: hidden.iter().map(|word| word.to_string()).collect(),
        }
    }

    fn default_words() -> Vec<String> {
        DEFAULT_MOODS.iter().map(|word| word.to_string()).collect()
    }

    // ---- 默认状态 / 加载 ----

    #[test]
    fn default_state_has_six_visible_defaults() {
        let state = State::default();
        let view = state.view();
        assert_eq!(view.visible, default_words());
        assert_eq!(view.defaults, default_words());
        assert!(view.custom.is_empty());
        assert!(view.hidden.is_empty());
        assert!(state.known_contains("走神"));
        assert!(!state.known_contains("摸鱼"));
    }

    #[test]
    fn legacy_config_without_moods_field_loads_empty() {
        let temp = TempDir::new("legacy");
        let path = temp.config_file();
        // 旧版 124B 形态：没有 moods 字段
        std::fs::write(
            &path,
            format!(
                "{{\n  \"api_key\": \"{PLACEHOLDER_KEY}\",\n  \"base_url\": \"https://api.deepseek.com\",\n  \"model\": \"deepseek-chat\"\n}}"
            ),
        )
        .expect("写旧版配置");
        let config = Config::load_from(&path);
        assert!(config.moods.is_none());
        assert_eq!(config.api_key.as_deref(), Some(PLACEHOLDER_KEY));

        let state = State::from_config(&config);
        assert_eq!(state.view().visible, default_words());
        assert!(state.view().custom.is_empty());
    }

    #[test]
    fn invalid_moods_field_is_treated_as_empty() {
        let temp = TempDir::new("invalid-moods");
        let path = temp.config_file();
        std::fs::write(
            &path,
            format!(
                "{{\"api_key\": \"{PLACEHOLDER_KEY}\", \"moods\": \"not-an-object\"}}"
            ),
        )
        .expect("写非法配置");
        let config = Config::load_from(&path);
        assert!(config.moods.is_none());
        assert_eq!(config.api_key.as_deref(), Some(PLACEHOLDER_KEY));
        assert_eq!(State::from_config(&config).view().visible, default_words());
    }

    #[test]
    fn loaded_config_is_sanitized() {
        // 手改配置可能带默认词、重复、非法词、未知 hidden：加载时全部清理。
        let temp = TempDir::new("sanitize");
        let path = temp.config_file();
        let config = sample_config(Some(MoodsConfig {
            custom: vec![
                "走神".to_string(),
                "  摸鱼  ".to_string(),
                "摸鱼".to_string(),
                "含 · 词".to_string(),
                "".to_string(),
            ],
            hidden: vec![
                "走神".to_string(),
                "摸鱼".to_string(),
                "未知词".to_string(),
                "走神".to_string(),
            ],
        }));
        write_config(&path, &config);

        let state = load_state(&path);
        let view = state.view();
        assert_eq!(view.custom, vec!["摸鱼".to_string()]);
        assert_eq!(view.hidden, vec!["走神".to_string(), "摸鱼".to_string()]);
        assert_eq!(
            view.visible,
            vec![
                "做成了".to_string(),
                "卡住".to_string(),
                "硬撑".to_string(),
                "空转".to_string(),
                "想躲".to_string(),
            ]
        );
        assert!(state.known_contains("走神"), "hidden 默认词仍在 known 中");
        assert!(state.known_contains("摸鱼"));
    }

    #[test]
    fn loaded_config_respects_visible_limit() {
        let temp = TempDir::new("limit-load");
        let path = temp.config_file();
        let custom: Vec<String> = (1..=30).map(|index| format!("词{index}")).collect();
        write_config(
            &path,
            &sample_config(Some(MoodsConfig {
                custom,
                hidden: Vec::new(),
            })),
        );

        let state = load_state(&path);
        assert_eq!(state.visible().len(), MAX_VISIBLE_MOODS);
    }

    // ---- 添加 ----

    #[test]
    fn add_appends_custom_after_defaults_and_persists() {
        let temp = TempDir::new("add");
        let path = temp.config_file();
        write_config(&path, &sample_config(None));

        let mut state = load_state(&path);
        let view = add_at(&path, &mut state, "摸鱼").expect("添加应成功");

        let mut expected = default_words();
        expected.push("摸鱼".to_string());
        assert_eq!(view.visible, expected);
        assert_eq!(view.custom, vec!["摸鱼".to_string()]);
        assert!(view.hidden.is_empty());
        assert!(state.known_contains("摸鱼"));

        // 写回后重新加载仍然一致
        let reloaded = load_state(&path);
        assert_eq!(reloaded.view(), view);
    }

    #[test]
    fn add_trims_surrounding_whitespace() {
        let temp = TempDir::new("trim");
        let path = temp.config_file();
        write_config(&path, &sample_config(None));
        let mut state = load_state(&path);

        let view = add_at(&path, &mut state, "  摸鱼  ").expect("添加应成功");
        assert_eq!(view.custom, vec!["摸鱼".to_string()]);
    }

    #[test]
    fn add_accepts_exactly_eight_chars() {
        let temp = TempDir::new("eight");
        let path = temp.config_file();
        write_config(&path, &sample_config(None));
        let mut state = load_state(&path);

        add_at(&path, &mut state, "一二三四五六七八").expect("8 个字应允许");
        let error = add_at(&path, &mut state, "一二三四五六七八九").expect_err("9 个字应拒绝");
        assert!(error.contains('8'), "错误应提示上限: {error}");
    }

    #[test]
    fn add_rejects_empty_word() {
        let temp = TempDir::new("empty");
        let path = temp.config_file();
        write_config(&path, &sample_config(None));
        let mut state = load_state(&path);

        for word in ["", "   ", "\t"] {
            let error = add_at(&path, &mut state, word).expect_err("空心情应拒绝");
            assert!(error.contains("不能为空") || error.contains("换行"), "实际错误: {error}");
        }
        assert!(state.view().custom.is_empty());
    }

    #[test]
    fn add_rejects_duplicate_visible_word() {
        let temp = TempDir::new("duplicate");
        let path = temp.config_file();
        write_config(&path, &sample_config(None));
        let mut state = load_state(&path);

        let error = add_at(&path, &mut state, "走神").expect_err("重名应拒绝");
        assert!(error.contains("已存在"), "实际错误: {error}");

        add_at(&path, &mut state, "摸鱼").expect("先添加自定义词");
        let error = add_at(&path, &mut state, "摸鱼").expect_err("自定义词重名应拒绝");
        assert!(error.contains("已存在"), "实际错误: {error}");
    }

    #[test]
    fn add_rejects_separator_newline_and_tab() {
        let temp = TempDir::new("bad-chars");
        let path = temp.config_file();
        write_config(&path, &sample_config(None));
        let mut state = load_state(&path);

        for word in ["摸 · 鱼", "摸\n鱼", "摸\r鱼", "摸\t鱼"] {
            let error = add_at(&path, &mut state, word).expect_err("非法字符应拒绝");
            assert!(
                error.contains("分隔符") || error.contains("换行"),
                "实际错误: {error}"
            );
        }
        assert!(state.view().custom.is_empty());
    }

    #[test]
    fn visible_limit_is_twenty() {
        let temp = TempDir::new("limit-add");
        let path = temp.config_file();
        write_config(&path, &sample_config(None));
        let mut state = load_state(&path);

        for index in 1..=14 {
            add_at(&path, &mut state, &format!("词{index}")).expect("前 14 个自定义词应允许");
        }
        assert_eq!(state.visible().len(), MAX_VISIBLE_MOODS);

        let error = add_at(&path, &mut state, "词15").expect_err("超过 20 个应拒绝");
        assert!(error.contains("20"), "实际错误: {error}");

        // 删除一个后可以再添加
        delete_at(&path, &mut state, "词1").expect("删除后应腾出名额");
        let view = add_at(&path, &mut state, "词15").expect("腾出名额后应允许添加");
        assert_eq!(view.visible.len(), MAX_VISIBLE_MOODS);
        assert!(!view.visible.contains(&"词1".to_string()));
    }

    #[test]
    fn restore_is_rejected_when_visible_limit_reached() {
        let temp = TempDir::new("limit-restore");
        let path = temp.config_file();
        write_config(&path, &sample_config(None));
        let mut state = load_state(&path);

        for index in 1..=14 {
            add_at(&path, &mut state, &format!("词{index}")).expect("前 14 个自定义词应允许");
        }
        delete_at(&path, &mut state, "词1").expect("删除后应腾出名额");
        add_at(&path, &mut state, "词15").expect("腾出名额后应允许添加");
        assert_eq!(state.visible().len(), MAX_VISIBLE_MOODS);

        let error = add_at(&path, &mut state, "词1").expect_err("可见已满时恢复也应拒绝");
        assert!(error.contains("20"), "实际错误: {error}");
        assert!(state.is_hidden("词1"), "被拒绝的恢复不得修改 hidden");

        delete_at(&path, &mut state, "词2").expect("再删一个就腾出名额");
        let view = add_at(&path, &mut state, "词1").expect("腾出名额后应能恢复");
        assert!(view.visible.contains(&"词1".to_string()));
        assert_eq!(view.visible.len(), MAX_VISIBLE_MOODS);
    }

    #[test]
    fn moods_view_serde_shape_matches_contract() {
        let state = State::from_config(&sample_config(Some(moods_config(
            &["摸鱼"],
            &["走神"],
        ))));
        let value = serde_json::to_value(state.view()).expect("序列化 MoodsView");
        assert_eq!(
            value,
            serde_json::json!({
                "visible": ["做成了", "卡住", "硬撑", "空转", "想躲", "摸鱼"],
                "defaults": ["做成了", "卡住", "硬撑", "走神", "空转", "想躲"],
                "custom": ["摸鱼"],
                "hidden": ["走神"]
            })
        );
    }

    #[test]
    fn persist_preserves_api_key_base_url_and_model() {
        let temp = TempDir::new("preserve");
        let path = temp.config_file();
        let mut config = sample_config(None);
        config.base_url = "http://127.0.0.1:9".to_string();
        config.model = "test-model".to_string();
        write_config(&path, &config);

        let mut state = load_state(&path);
        add_at(&path, &mut state, "摸鱼").expect("添加应成功");

        let reloaded = Config::load_from(&path);
        assert_eq!(reloaded.api_key.as_deref(), Some(PLACEHOLDER_KEY));
        assert_eq!(reloaded.base_url, "http://127.0.0.1:9");
        assert_eq!(reloaded.model, "test-model");
        assert_eq!(
            reloaded.moods,
            Some(MoodsConfig {
                custom: vec!["摸鱼".to_string()],
                hidden: Vec::new(),
            })
        );
    }

    #[test]
    fn failed_persist_does_not_change_memory_state() {
        let temp = TempDir::new("rollback");
        let path = temp.config_file();
        // 用目录占住 config.json 路径：写入必然失败
        std::fs::create_dir_all(&path).expect("用目录占住配置路径");

        let mut state = State::default();
        let error = add_at(&path, &mut state, "摸鱼").expect_err("写回失败应报错");
        assert!(!error.is_empty());
        assert!(state.view().custom.is_empty(), "写回失败不得更新内存");
        assert_eq!(state.view().visible, default_words());
    }

    // ---- 删除 / 恢复 ----

    #[test]
    fn delete_default_hides_but_keeps_it_known() {
        let temp = TempDir::new("delete-default");
        let path = temp.config_file();
        write_config(&path, &sample_config(None));
        let mut state = load_state(&path);

        let view = delete_at(&path, &mut state, "走神").expect("删除默认词应成功");
        assert!(!view.visible.contains(&"走神".to_string()));
        assert_eq!(view.hidden, vec!["走神".to_string()]);
        assert!(view.custom.is_empty(), "默认词不进 custom");
        assert!(state.known_contains("走神"), "hidden 词仍在 known 中");

        let reloaded = load_state(&path);
        assert_eq!(reloaded.view(), view);
    }

    #[test]
    fn delete_custom_keeps_custom_and_known() {
        let temp = TempDir::new("delete-custom");
        let path = temp.config_file();
        write_config(&path, &sample_config(Some(moods_config(&["摸鱼"], &[]))));
        let mut state = load_state(&path);

        let view = delete_at(&path, &mut state, "摸鱼").expect("删除自定义词应成功");
        assert!(!view.visible.contains(&"摸鱼".to_string()));
        assert_eq!(view.custom, vec!["摸鱼".to_string()], "只加 hidden，不删 custom");
        assert_eq!(view.hidden, vec!["摸鱼".to_string()]);
        assert!(state.known_contains("摸鱼"));
    }

    #[test]
    fn add_restores_hidden_default_mood() {
        let temp = TempDir::new("restore-default");
        let path = temp.config_file();
        write_config(&path, &sample_config(Some(moods_config(&[], &["走神"]))));
        let mut state = load_state(&path);
        assert!(!state.view().visible.contains(&"走神".to_string()));

        let view = add_at(&path, &mut state, "走神").expect("恢复隐藏默认词应成功");
        assert!(view.visible.contains(&"走神".to_string()));
        assert!(view.hidden.is_empty());
        assert!(view.custom.is_empty(), "恢复默认词不进 custom");
    }

    #[test]
    fn add_restores_hidden_custom_mood() {
        let temp = TempDir::new("restore-custom");
        let path = temp.config_file();
        write_config(
            &path,
            &sample_config(Some(moods_config(&["摸鱼"], &["摸鱼"]))),
        );
        let mut state = load_state(&path);
        assert!(!state.view().visible.contains(&"摸鱼".to_string()));

        let view = add_at(&path, &mut state, "摸鱼").expect("恢复隐藏自定义词应成功");
        let mut expected = default_words();
        expected.push("摸鱼".to_string());
        assert_eq!(view.visible, expected, "恢复后仍按自定义顺序排在默认词后");
        assert!(view.hidden.is_empty());
        assert_eq!(view.custom, vec!["摸鱼".to_string()]);
    }

    #[test]
    fn delete_unknown_or_already_hidden_word_errors() {
        let temp = TempDir::new("delete-unknown");
        let path = temp.config_file();
        write_config(&path, &sample_config(None));
        let mut state = load_state(&path);

        let error = delete_at(&path, &mut state, "摸鱼").expect_err("未知词应拒绝");
        assert!(error.contains("不在可见"), "实际错误: {error}");

        delete_at(&path, &mut state, "走神").expect("第一次删除应成功");
        let error = delete_at(&path, &mut state, "走神").expect_err("重复删除应拒绝");
        assert!(error.contains("不在可见"), "实际错误: {error}");
        assert_eq!(state.view().hidden, vec!["走神".to_string()]);
    }

    // ---- 隐藏词仍参与解析 ----

    #[test]
    fn hidden_default_mood_is_still_parsed_as_mood() {
        let temp = TempDir::new("parse-hidden");
        let path = temp.config_file();
        write_config(&path, &sample_config(Some(moods_config(&[], &["走神"]))));
        let state = load_state(&path);

        assert!(!state.visible().contains(&"走神".to_string()));
        let known = state.known_words();
        let line = format_line("16:30", "便利店，买了冰咖啡", Some(Mood::custom("走神").unwrap()));
        assert_eq!(line, "16:30 · 便利店，买了冰咖啡 · 走神");

        let entry = parse_line_with(&line, |word| known.contains(&word)).expect("应能解析");
        assert_eq!(entry.content, "便利店，买了冰咖啡");
        assert_eq!(entry.mood, Some(Mood::Distracted), "隐藏词仍应识别为心情");
    }
}
