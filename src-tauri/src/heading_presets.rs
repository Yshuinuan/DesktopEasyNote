//! 预设标题数据层（任务书 R15）。
//!
//! 用户常用 `#` 标题记录工作问题，前端提供「点一下插入一个预设标题」；每条预设 = 级别 + 文字。
//!
//! - 默认四条（级别均为 2）：问题 / 思路 / 解决 / 复盘；`config.json` 没有 `heading_presets` 字段时使用
//! - 顺序 = 数组顺序：默认四条在前，新增追加到末尾
//! - 校验：`text` trim 后非空、Unicode 字符数 ≤ 20、不含换行/制表符/` · `；`level` 1–6；
//!   同一 `text` 不得重复（不论级别）；总数上限 20
//! - 启动时加载一次并缓存；增删改后写回 `config.json` 并更新内存；写回失败内存回滚；`RwLock` 保证线程安全
//! - 写回前完整重读配置，不丢失 `api_key`/`base_url`/`model`/`moods`
//! - 本模块不读取、不打印 apikey 明文

use std::path::Path;
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::record_line::SEPARATOR;

/// 默认预设标题文字（R15：顺序固定）。
pub const DEFAULT_PRESET_WORDS: [&str; 4] = ["问题", "思路", "解决", "复盘"];

/// 默认预设标题级别（四条均为 2）。
pub const DEFAULT_PRESET_LEVEL: u8 = 2;

/// 单条预设标题文字的 Unicode 字符数上限。
pub const MAX_PRESET_TEXT_CHARS: usize = 20;

/// 预设标题总数上限。
pub const MAX_HEADING_PRESETS: usize = 20;

/// 一条预设标题：Markdown 标题级别 1–6 + 文字；serde 字段名 `level`/`text`（与前端 TS `HeadingPreset` 一致）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadingPreset {
    pub level: u8,
    pub text: String,
}

/// 默认四条预设（级别均为 2，顺序固定）。
pub fn default_presets() -> Vec<HeadingPreset> {
    DEFAULT_PRESET_WORDS
        .iter()
        .map(|text| HeadingPreset {
            level: DEFAULT_PRESET_LEVEL,
            text: (*text).to_string(),
        })
        .collect()
}

/// 校验并规范化文字（trim）：非空、≤20 个 Unicode 字符、不含 ` · `/换行/制表符。
fn validate_text(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("预设标题不能为空".to_string());
    }
    if text.chars().count() > MAX_PRESET_TEXT_CHARS {
        return Err(format!("预设标题最多 {MAX_PRESET_TEXT_CHARS} 个字"));
    }
    if text.contains(SEPARATOR) {
        return Err(format!("预设标题不能包含分隔符「{SEPARATOR}」"));
    }
    if text.contains(['\n', '\r', '\t']) {
        return Err("预设标题不能包含换行或制表符".to_string());
    }
    Ok(text.to_string())
}

/// 校验级别 1–6。
fn validate_level(level: u8) -> Result<(), String> {
    if (1..=6).contains(&level) {
        Ok(())
    } else {
        Err(format!("标题级别必须是 1–6（收到 {level}）"))
    }
}

/// 校验并规范化一条预设；返回 trim 后的文字与级别。
fn validate_preset(text: &str, level: u8) -> Result<HeadingPreset, String> {
    let text = validate_text(text)?;
    validate_level(level)?;
    Ok(HeadingPreset { level, text })
}

/// 清洗配置里的预设：丢弃非法/重复条目、最多保留 20 条。
///
/// 配置非空但一条合法条目都没有时，按「配置损坏」回退默认四条；
/// `Some([])`（用户把预设全删光）是显式空列表，保持为空、不回填默认。
fn sanitize_presets(raw: &[HeadingPreset]) -> Vec<HeadingPreset> {
    let mut presets: Vec<HeadingPreset> = Vec::new();
    for item in raw {
        let Ok(preset) = validate_preset(&item.text, item.level) else {
            continue;
        };
        if presets.iter().any(|existing| existing.text == preset.text) {
            continue;
        }
        if presets.len() >= MAX_HEADING_PRESETS {
            break;
        }
        presets.push(preset);
    }
    if presets.is_empty() && !raw.is_empty() {
        return default_presets();
    }
    presets
}

/// 内存中的预设标题状态（顺序即数组顺序）。
#[derive(Debug, Clone)]
struct State {
    presets: Vec<HeadingPreset>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            presets: default_presets(),
        }
    }
}

impl State {
    /// 从配置构建：字段缺失/null → 默认四条；字段存在 → 按内容清洗（可能为空）。
    fn from_config(config: &Config) -> Self {
        match config.heading_presets.as_ref() {
            None => Self::default(),
            Some(raw) => Self {
                presets: sanitize_presets(raw),
            },
        }
    }

    fn list(&self) -> Vec<HeadingPreset> {
        self.presets.clone()
    }

    fn add(&mut self, preset: HeadingPreset) -> Result<(), String> {
        if self
            .presets
            .iter()
            .any(|existing| existing.text == preset.text)
        {
            return Err(format!("预设标题「{}」已存在", preset.text));
        }
        if self.presets.len() >= MAX_HEADING_PRESETS {
            return Err(format!(
                "预设标题最多 {MAX_HEADING_PRESETS} 条，请先删除一条再添加"
            ));
        }
        self.presets.push(preset);
        Ok(())
    }

    fn update(&mut self, index: usize, preset: HeadingPreset) -> Result<(), String> {
        let len = self.presets.len();
        if index >= len {
            return Err(format!("预设标题不存在（索引 {index}，共 {len} 条）"));
        }
        if self
            .presets
            .iter()
            .enumerate()
            .any(|(existing_index, existing)| existing_index != index && existing.text == preset.text)
        {
            return Err(format!("预设标题「{}」已存在", preset.text));
        }
        self.presets[index] = preset;
        Ok(())
    }

    fn delete(&mut self, index: usize) -> Result<(), String> {
        let len = self.presets.len();
        if index >= len {
            return Err(format!("预设标题不存在（索引 {index}，共 {len} 条）"));
        }
        self.presets.remove(index);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 全局缓存（启动加载一次；增删改后写回并更新）
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
/// 字段缺失时使用默认四条；不读取 key，只记录条数。
pub fn init(config: &Config) {
    let loaded = State::from_config(config);
    let count = loaded.presets.len();
    write_state(|state| *state = loaded);
    crate::log_info!("预设标题配置加载完成（{count} 条）");
}

/// 当前预设标题列表（数组顺序；默认四条在前、新增追加末尾）。
pub fn list() -> Vec<HeadingPreset> {
    read_state(State::list)
}

/// 新增一条预设（追加到末尾）。成功返回操作后的完整列表。
pub fn add(text: &str, level: u8) -> Result<Vec<HeadingPreset>, String> {
    let config_file = crate::paths::get().config_file.clone();
    let presets = write_state(|state| add_at(&config_file, state, text, level))?;
    crate::log_info!(
        "预设标题：新增「{}」（级别 {level}，当前 {} 条）",
        text.trim(),
        presets.len()
    );
    Ok(presets)
}

/// 按索引（0 起）修改一条预设，位置不变。成功返回操作后的完整列表。
pub fn update(index: usize, text: &str, level: u8) -> Result<Vec<HeadingPreset>, String> {
    let config_file = crate::paths::get().config_file.clone();
    let presets = write_state(|state| update_at(&config_file, state, index, text, level))?;
    crate::log_info!(
        "预设标题：修改索引 {index} 为「{}」（级别 {level}，当前 {} 条）",
        text.trim(),
        presets.len()
    );
    Ok(presets)
}

/// 按索引（0 起）删除一条预设。成功返回操作后的完整列表。
pub fn delete(index: usize) -> Result<Vec<HeadingPreset>, String> {
    let config_file = crate::paths::get().config_file.clone();
    let presets = write_state(|state| delete_at(&config_file, state, index))?;
    crate::log_info!(
        "预设标题：删除索引 {index}（当前 {} 条）",
        presets.len()
    );
    Ok(presets)
}

// ---------------------------------------------------------------------------
// 可注入配置路径的内部实现（单测用临时 config，不碰真实配置）
// ---------------------------------------------------------------------------

fn add_at(
    config_file: &Path,
    state: &mut State,
    text: &str,
    level: u8,
) -> Result<Vec<HeadingPreset>, String> {
    let preset = validate_preset(text, level)?;
    let mut candidate = state.clone();
    candidate.add(preset)?;
    persist_to(config_file, &candidate)?;
    *state = candidate;
    Ok(state.list())
}

fn update_at(
    config_file: &Path,
    state: &mut State,
    index: usize,
    text: &str,
    level: u8,
) -> Result<Vec<HeadingPreset>, String> {
    let preset = validate_preset(text, level)?;
    let mut candidate = state.clone();
    candidate.update(index, preset)?;
    persist_to(config_file, &candidate)?;
    *state = candidate;
    Ok(state.list())
}

fn delete_at(
    config_file: &Path,
    state: &mut State,
    index: usize,
) -> Result<Vec<HeadingPreset>, String> {
    let mut candidate = state.clone();
    candidate.delete(index)?;
    persist_to(config_file, &candidate)?;
    *state = candidate;
    Ok(state.list())
}

/// 写回配置：先完整重读（保留 api_key/base_url/model/moods），只替换 `heading_presets` 字段。
fn persist_to(config_file: &Path, state: &State) -> Result<(), String> {
    let mut config = Config::load_from(config_file);
    config.heading_presets = Some(state.list());
    config.save_to(config_file)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::config::MoodsConfig;

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
                "desktop-narrator-presets-{tag}-{}-{seq}",
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

    fn preset(level: u8, text: &str) -> HeadingPreset {
        HeadingPreset {
            level,
            text: text.to_string(),
        }
    }

    fn texts(presets: &[HeadingPreset]) -> Vec<&str> {
        presets.iter().map(|preset| preset.text.as_str()).collect()
    }

    /// 完整配置：带假 key + 自定义心情，用于验证写回不丢这些字段。
    fn base_config() -> Config {
        Config {
            api_key: Some(PLACEHOLDER_KEY.to_string()),
            base_url: "https://api.deepseek.com".to_string(),
            model: "deepseek-chat".to_string(),
            moods: Some(MoodsConfig {
                custom: vec!["摸鱼".to_string()],
                hidden: vec!["走神".to_string()],
            }),
            heading_presets: None,
        }
    }

    fn write_config(path: &Path, config: &Config) {
        config.save_to(path).expect("写入测试配置");
    }

    fn load_state(path: &Path) -> State {
        State::from_config(&Config::load_from(path))
    }

    // ---- 默认值 / 加载 ----

    #[test]
    fn default_presets_are_four_level_two_in_order() {
        assert_eq!(
            default_presets(),
            vec![
                preset(2, "问题"),
                preset(2, "思路"),
                preset(2, "解决"),
                preset(2, "复盘"),
            ]
        );
    }

    #[test]
    fn config_without_heading_presets_uses_default_four() {
        let temp = TempDir::new("legacy");
        let path = temp.config_file();
        // 旧版 config.json（无 heading_presets 字段）可直接加载
        std::fs::write(
            &path,
            format!(
                "{{\n  \"api_key\": \"{PLACEHOLDER_KEY}\",\n  \"base_url\": \"https://api.deepseek.com\",\n  \"model\": \"deepseek-chat\"\n}}"
            ),
        )
        .expect("写旧版配置");

        let config = Config::load_from(&path);
        assert!(config.heading_presets.is_none());
        assert_eq!(config.api_key.as_deref(), Some(PLACEHOLDER_KEY));

        let state = State::from_config(&config);
        assert_eq!(state.list(), default_presets());
    }

    #[test]
    fn explicit_empty_presets_are_kept() {
        let temp = TempDir::new("empty");
        let path = temp.config_file();
        let mut config = base_config();
        config.heading_presets = Some(Vec::new());
        write_config(&path, &config);

        let state = load_state(&path);
        assert!(state.list().is_empty(), "显式空列表不应回填默认四条");
    }

    #[test]
    fn invalid_presets_in_config_are_sanitized() {
        let temp = TempDir::new("sanitize");
        let path = temp.config_file();
        let mut config = base_config();
        config.heading_presets = Some(vec![
            preset(0, "级别太低"),
            preset(7, "级别太高"),
            preset(2, ""),
            preset(2, "   "),
            preset(2, "含 · 分隔符"),
            preset(2, "含\n换行"),
            preset(3, "一二三四五六七八九十一二三四五六七八九十一"),
            preset(2, "问题"),
            preset(5, "问题"),
            preset(2, "新标题"),
        ]);
        write_config(&path, &config);

        let state = load_state(&path);
        assert_eq!(state.list(), vec![preset(2, "问题"), preset(2, "新标题")]);
    }

    #[test]
    fn all_invalid_presets_fall_back_to_defaults() {
        let temp = TempDir::new("all-invalid");
        let path = temp.config_file();
        let mut config = base_config();
        config.heading_presets = Some(vec![preset(0, "坏"), preset(2, "")]);
        write_config(&path, &config);

        assert_eq!(load_state(&path).list(), default_presets());
    }

    #[test]
    fn presets_beyond_limit_are_truncated() {
        let temp = TempDir::new("truncate");
        let path = temp.config_file();
        let mut config = base_config();
        config.heading_presets = Some((1..=30).map(|index| preset(2, &format!("词{index}"))).collect());
        write_config(&path, &config);

        let list = load_state(&path).list();
        assert_eq!(list.len(), MAX_HEADING_PRESETS);
        assert_eq!(list[0], preset(2, "词1"));
        assert_eq!(list[MAX_HEADING_PRESETS - 1], preset(2, "词20"));
    }

    // ---- 新增 ----

    #[test]
    fn add_appends_to_end_and_persists() {
        let temp = TempDir::new("add");
        let path = temp.config_file();
        write_config(&path, &base_config());
        let mut state = load_state(&path);

        let list = add_at(&path, &mut state, "验证", 3).expect("新增应成功");
        let mut expected = default_presets();
        expected.push(preset(3, "验证"));
        assert_eq!(list, expected);
        assert_eq!(texts(&list), vec!["问题", "思路", "解决", "复盘", "验证"]);

        let reloaded = load_state(&path);
        assert_eq!(reloaded.list(), expected);
    }

    #[test]
    fn add_trims_text() {
        let temp = TempDir::new("trim");
        let path = temp.config_file();
        write_config(&path, &base_config());
        let mut state = load_state(&path);

        let list = add_at(&path, &mut state, "  验证  ", 2).expect("新增应成功");
        assert_eq!(list.last(), Some(&preset(2, "验证")));
    }

    #[test]
    fn add_accepts_exactly_twenty_chars() {
        let temp = TempDir::new("twenty");
        let path = temp.config_file();
        write_config(&path, &base_config());
        let mut state = load_state(&path);

        add_at(
            &path,
            &mut state,
            "一二三四五六七八九十一二三四五六七八九十",
            2,
        )
        .expect("20 个字应允许");
        let error = add_at(
            &path,
            &mut state,
            "一二三四五六七八九十一二三四五六七八九十一",
            2,
        )
        .expect_err("21 个字应拒绝");
        assert!(error.contains("20"), "错误应提示上限: {error}");
    }

    #[test]
    fn add_rejects_empty_text() {
        let temp = TempDir::new("empty-text");
        let path = temp.config_file();
        write_config(&path, &base_config());
        let mut state = load_state(&path);

        for text in ["", "   "] {
            let error = add_at(&path, &mut state, text, 2).expect_err("空文字应拒绝");
            assert!(error.contains("不能为空"), "实际错误: {error}");
        }
        assert_eq!(state.list(), default_presets());
    }

    #[test]
    fn add_rejects_newline_tab_and_separator() {
        let temp = TempDir::new("bad-chars");
        let path = temp.config_file();
        write_config(&path, &base_config());
        let mut state = load_state(&path);

        for text in ["含 · 分隔符", "含\n换行", "含\r回车", "含\t制表符"] {
            let error = add_at(&path, &mut state, text, 2).expect_err("非法字符应拒绝");
            assert!(
                error.contains("分隔符") || error.contains("换行"),
                "实际错误: {error}"
            );
        }
        assert_eq!(state.list(), default_presets());
    }

    #[test]
    fn add_rejects_invalid_level() {
        let temp = TempDir::new("bad-level");
        let path = temp.config_file();
        write_config(&path, &base_config());
        let mut state = load_state(&path);

        for level in [0u8, 7, 200] {
            let error = add_at(&path, &mut state, "新标题", level).expect_err("级别越界应拒绝");
            assert!(error.contains("1–6") || error.contains("1-6"), "实际错误: {error}");
        }
        assert_eq!(state.list(), default_presets());
    }

    #[test]
    fn add_rejects_duplicate_text_regardless_of_level() {
        let temp = TempDir::new("duplicate");
        let path = temp.config_file();
        write_config(&path, &base_config());
        let mut state = load_state(&path);

        let error = add_at(&path, &mut state, "思路", 4).expect_err("重名应拒绝（不论级别）");
        assert!(error.contains("已存在"), "实际错误: {error}");
        assert_eq!(state.list(), default_presets());
    }

    #[test]
    fn add_respects_total_limit() {
        let temp = TempDir::new("limit-add");
        let path = temp.config_file();
        let mut config = base_config();
        config.heading_presets = Some((1..=20).map(|index| preset(2, &format!("词{index}"))).collect());
        write_config(&path, &config);
        let mut state = load_state(&path);

        let error = add_at(&path, &mut state, "词21", 2).expect_err("超过 20 条应拒绝");
        assert!(error.contains("20"), "实际错误: {error}");

        delete_at(&path, &mut state, 0).expect("删除后应腾出名额");
        let list = add_at(&path, &mut state, "词21", 2).expect("腾出名额后应允许");
        assert_eq!(list.len(), MAX_HEADING_PRESETS);
        assert_eq!(list.last(), Some(&preset(2, "词21")));
    }

    // ---- 修改 ----

    #[test]
    fn update_replaces_in_place_and_persists() {
        let temp = TempDir::new("update");
        let path = temp.config_file();
        write_config(&path, &base_config());
        let mut state = load_state(&path);

        let list = update_at(&path, &mut state, 1, "新思路", 4).expect("修改应成功");
        assert_eq!(
            list,
            vec![
                preset(2, "问题"),
                preset(4, "新思路"),
                preset(2, "解决"),
                preset(2, "复盘"),
            ],
            "位置应保持不变"
        );
        assert_eq!(load_state(&path).list(), list);
    }

    #[test]
    fn update_allows_same_text_when_only_level_changes() {
        let temp = TempDir::new("update-same-text");
        let path = temp.config_file();
        write_config(&path, &base_config());
        let mut state = load_state(&path);

        let list = update_at(&path, &mut state, 0, "问题", 5).expect("同位置同名应允许改级别");
        assert_eq!(list[0], preset(5, "问题"));
    }

    #[test]
    fn update_rejects_duplicate_text_of_other_index() {
        let temp = TempDir::new("update-duplicate");
        let path = temp.config_file();
        write_config(&path, &base_config());
        let mut state = load_state(&path);

        let error = update_at(&path, &mut state, 0, "思路", 2).expect_err("与其他条目重名应拒绝");
        assert!(error.contains("已存在"), "实际错误: {error}");
        assert_eq!(state.list(), default_presets(), "失败不得修改内存");
    }

    #[test]
    fn update_rejects_out_of_bounds_index() {
        let temp = TempDir::new("update-oob");
        let path = temp.config_file();
        write_config(&path, &base_config());
        let mut state = load_state(&path);

        let error = update_at(&path, &mut state, 4, "新标题", 2).expect_err("索引越界应拒绝");
        assert!(error.contains("索引"), "实际错误: {error}");
        assert_eq!(state.list(), default_presets());
    }

    // ---- 删除 ----

    #[test]
    fn delete_removes_index_and_keeps_order() {
        let temp = TempDir::new("delete");
        let path = temp.config_file();
        write_config(&path, &base_config());
        let mut state = load_state(&path);

        let list = delete_at(&path, &mut state, 1).expect("删除应成功");
        let expected = vec![preset(2, "问题"), preset(2, "解决"), preset(2, "复盘")];
        assert_eq!(list, expected);
        assert_eq!(load_state(&path).list(), expected);
    }

    #[test]
    fn delete_all_keeps_explicit_empty_list() {
        let temp = TempDir::new("delete-all");
        let path = temp.config_file();
        write_config(&path, &base_config());
        let mut state = load_state(&path);

        for _ in 0..4 {
            delete_at(&path, &mut state, 0).expect("逐条删除应成功");
        }
        assert!(state.list().is_empty());

        let reloaded = load_state(&path);
        assert!(reloaded.list().is_empty(), "删光后重启不应回填默认四条");
    }

    #[test]
    fn delete_rejects_out_of_bounds_index() {
        let temp = TempDir::new("delete-oob");
        let path = temp.config_file();
        write_config(&path, &base_config());
        let mut state = load_state(&path);

        let error = delete_at(&path, &mut state, 4).expect_err("索引越界应拒绝");
        assert!(error.contains("索引"), "实际错误: {error}");
        assert_eq!(state.list(), default_presets());
    }

    // ---- 持久化安全性 ----

    #[test]
    fn persist_preserves_api_key_moods_base_url_and_model() {
        let temp = TempDir::new("preserve");
        let path = temp.config_file();
        let mut config = base_config();
        config.base_url = "http://127.0.0.1:9".to_string();
        config.model = "test-model".to_string();
        write_config(&path, &config);
        let mut state = load_state(&path);

        add_at(&path, &mut state, "验证", 2).expect("新增应成功");

        let reloaded = Config::load_from(&path);
        assert_eq!(reloaded.api_key.as_deref(), Some(PLACEHOLDER_KEY));
        assert_eq!(reloaded.base_url, "http://127.0.0.1:9");
        assert_eq!(reloaded.model, "test-model");
        assert_eq!(
            reloaded.moods,
            Some(MoodsConfig {
                custom: vec!["摸鱼".to_string()],
                hidden: vec!["走神".to_string()],
            }),
            "写回预设不得丢 moods"
        );
        let mut expected = default_presets();
        expected.push(preset(2, "验证"));
        assert_eq!(reloaded.heading_presets, Some(expected));
    }

    #[test]
    fn failed_persist_rolls_back_memory_state() {
        let temp = TempDir::new("rollback");
        let path = temp.config_file();
        // 用目录占住 config.json 路径：写入必然失败
        std::fs::create_dir_all(&path).expect("用目录占住配置路径");

        let mut state = State::default();
        let error = add_at(&path, &mut state, "验证", 2).expect_err("写回失败应报错");
        assert!(!error.is_empty());
        assert_eq!(state.list(), default_presets(), "写回失败不得更新内存");
    }

    // ---- serde 形状 ----

    #[test]
    fn preset_serde_shape_matches_contract() {
        let value = serde_json::to_value(preset(2, "问题")).expect("序列化预设");
        assert_eq!(value, serde_json::json!({ "level": 2, "text": "问题" }));

        let parsed: HeadingPreset = serde_json::from_value(value).expect("反序列化预设");
        assert_eq!(parsed, preset(2, "问题"));

        let list = serde_json::to_value(vec![preset(3, "验证")]).expect("序列化列表");
        assert_eq!(list, serde_json::json!([{ "level": 3, "text": "验证" }]));
    }
}
