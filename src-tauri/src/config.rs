//! 配置存储：`config.json`（apikey / baseURL / model / moods / heading_presets）。
//!
//! 安全约束：apikey 只落本机配置文件，日志/错误信息/前端 console 里绝不出现明文；
//! 前端只能拿到 `SettingsView { has_api_key }`，拿不到 key 本身。
//!
//! R14：新增可选 `moods` 字段（自定义/隐藏心情）。缺失或非法时按空处理；
//! 只有用户增删心情时才写回，且写回前完整重读，不丢失 `api_key`/`base_url`/`model`。
//! R15：新增可选 `heading_presets` 字段（预设标题）。缺失或非法时由数据层使用默认四条；
//! 只有增删改预设时才写回，写回前完整重读，不丢失 api_key/base_url/model/moods。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::heading_presets::HeadingPreset;
use crate::paths;

pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
pub const DEFAULT_MODEL: &str = "deepseek-chat";

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_string()
}

fn default_model() -> String {
    DEFAULT_MODEL.to_string()
}

/// `config.json` 里的 `moods` 字段（R14）：用户自定义词 + 被删除（隐藏）的词。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoodsConfig {
    /// 用户添加的自定义心情词（按添加顺序，含被隐藏的）。
    #[serde(default)]
    pub custom: Vec<String>,
    /// 被删除（仅隐藏）的心情词，可能含默认六词。
    #[serde(default)]
    pub hidden: Vec<String>,
}

/// `moods` 字段缺失或非法时都退化为「无自定义/无隐藏」，不影响同文件里的 key 等字段。
///
/// 先把字段值整体读成 `serde_json::Value` 再宽松解析：这样即使形状非法（字符串/数组/坏对象），
/// 也不会像「吞掉反序列化错误」那样把外层 JSON 解析器的状态弄坏。
fn deserialize_moods_lenient<'de, D>(deserializer: D) -> Result<Option<MoodsConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value::<MoodsConfig>(value).ok())
}

/// `heading_presets` 字段缺失或非法时都退化为 `None`（由数据层使用默认四条），
/// 不影响同文件里的 key / moods 等字段。同样先整体读成 `serde_json::Value` 再解析。
fn deserialize_heading_presets_lenient<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<HeadingPreset>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value::<Vec<HeadingPreset>>(value).ok())
}

/// `config.json` 的完整结构。JSON key 使用 snake_case：
/// `api_key` / `base_url` / `model` / `moods` / `heading_presets`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_model")]
    pub model: String,
    /// R14 自定义心情；`None` 表示配置文件里没有该字段（旧版配置）。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_moods_lenient"
    )]
    pub moods: Option<MoodsConfig>,
    /// R15 预设标题；`None` 表示配置文件里没有该字段（加载时使用默认四条）。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_heading_presets_lenient"
    )]
    pub heading_presets: Option<Vec<HeadingPreset>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            moods: None,
            heading_presets: None,
        }
    }
}

/// 给前端的设置视图，永远不含 key 明文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsView {
    pub has_api_key: bool,
    pub base_url: String,
    pub model: String,
}

impl Config {
    /// 读取配置：文件缺失时写出一份默认配置；解析失败时回退默认值并写日志。
    pub fn load() -> Self {
        Self::load_from(&paths::get().config_file)
    }

    /// 从指定路径读取配置（单测用临时目录，不碰真实配置）。
    pub(crate) fn load_from(path: &Path) -> Self {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let config = Self::default();
                if let Err(save_error) = config.save_to(path) {
                    crate::log_warn!("写入默认配置失败: {save_error}");
                }
                return config;
            }
            Err(error) => {
                crate::log_warn!("读取配置失败，使用默认配置: {error}");
                return Self::default();
            }
        };

        match serde_json::from_str::<Self>(&raw) {
            Ok(config) => config,
            Err(error) => {
                // 只记录解析错误位置，不打印原始内容（可能含 key）
                crate::log_warn!("config.json 解析失败，使用默认配置: {error}");
                Self::default()
            }
        }
    }

    /// 原子式写入：先写临时文件，再替换正式文件。返回可读错误信息（不含 key）。
    pub fn save(&self) -> Result<(), String> {
        self.save_to(&paths::get().config_file)
    }

    /// 写入指定路径（单测用临时目录，不碰真实配置）。
    pub(crate) fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| format!("创建配置目录失败: {error}"))?;
        }

        let json = serde_json::to_string_pretty(self)
            .map_err(|error| format!("序列化配置失败: {error}"))?;
        let tmp_path = path.with_extension("json.tmp");

        std::fs::write(&tmp_path, json).map_err(|error| format!("写入临时配置失败: {error}"))?;
        // Windows 上 rename 不能覆盖已有文件，先删旧的再替换
        if path.exists() {
            std::fs::remove_file(path).map_err(|error| format!("替换配置失败: {error}"))?;
        }
        std::fs::rename(&tmp_path, path).map_err(|error| format!("替换配置失败: {error}"))?;
        Ok(())
    }

    /// 保存 apikey（空值拒绝；值不写日志）。供 `save_api_key` 命令调用。
    pub fn save_api_key(key: &str) -> Result<(), String> {
        let trimmed = key.trim();
        if trimmed.is_empty() {
            return Err("API key 不能为空".to_string());
        }
        if trimmed.len() > 4096 {
            return Err("API key 过长".to_string());
        }

        let mut config = Self::load();
        config.api_key = Some(trimmed.to_string());
        config.save()?;
        crate::log_info!("已保存 API key（值不写日志）");
        Ok(())
    }

    pub fn settings_view(&self) -> SettingsView {
        SettingsView {
            has_api_key: self
                .api_key
                .as_deref()
                .is_some_and(|key| !key.trim().is_empty()),
            base_url: self.base_url.clone(),
            model: self.model.clone(),
        }
    }

    /// 供 P2 读取 key；调用方不得打印返回值。
    pub fn api_key(&self) -> Option<String> {
        self.api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values_follow_planning_doc() {
        let config = Config::default();
        assert_eq!(config.base_url, "https://api.deepseek.com");
        assert_eq!(config.model, "deepseek-chat");
        assert!(config.api_key.is_none());
        assert!(config.moods.is_none());
        assert!(config.heading_presets.is_none());
    }

    #[test]
    fn settings_view_never_exposes_key() {
        let mut config = Config::default();
        assert!(!config.settings_view().has_api_key);

        config.api_key = Some("placeholder-not-a-real-key".to_string());
        let view = config.settings_view();
        assert!(view.has_api_key);
        let json = serde_json::to_string(&view).expect("序列化 SettingsView");
        assert!(!json.contains("placeholder-not-a-real-key"));
        // 不得出现 api_key 字段名（has_api_key 是允许的）
        assert!(!json.contains("\"api_key\""));
    }

    #[test]
    fn config_json_roundtrip_uses_snake_case_keys() {
        let config = Config {
            api_key: Some("placeholder-not-a-real-key".to_string()),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            moods: Some(MoodsConfig {
                custom: vec!["摸鱼".to_string()],
                hidden: vec!["走神".to_string()],
            }),
            heading_presets: None,
        };
        let json = serde_json::to_value(&config).expect("序列化配置");
        assert_eq!(json["base_url"], DEFAULT_BASE_URL);
        assert_eq!(json["model"], DEFAULT_MODEL);
        assert_eq!(json["api_key"], "placeholder-not-a-real-key");
        assert_eq!(json["moods"]["custom"], serde_json::json!(["摸鱼"]));
        assert_eq!(json["moods"]["hidden"], serde_json::json!(["走神"]));

        let parsed: Config = serde_json::from_value(json).expect("反序列化配置");
        assert_eq!(parsed, config);
    }

    #[test]
    fn empty_api_key_is_not_treated_as_saved() {
        let mut config = Config::default();
        config.api_key = Some("   ".to_string());
        assert!(!config.settings_view().has_api_key);
        assert!(config.api_key().is_none());
    }

    // ---- R14：moods 可选字段与兼容性 ----

    #[test]
    fn legacy_config_without_moods_field_loads() {
        // 旧版 config.json（无 moods 字段，124B 形态）：不因缺字段报错
        let raw = r#"{
  "api_key": "placeholder-not-a-real-key",
  "base_url": "https://api.deepseek.com",
  "model": "deepseek-chat"
}"#;
        let config: Config = serde_json::from_str(raw).expect("旧版配置应能加载");
        assert_eq!(config.api_key.as_deref(), Some("placeholder-not-a-real-key"));
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.model, DEFAULT_MODEL);
        assert!(config.moods.is_none());
    }

    #[test]
    fn invalid_moods_field_is_treated_as_empty() {
        for raw in [
            r#"{"api_key": "placeholder-not-a-real-key", "moods": "not-an-object"}"#,
            r#"{"api_key": "placeholder-not-a-real-key", "moods": null}"#,
            r#"{"api_key": "placeholder-not-a-real-key", "moods": {"custom": "bad"}}"#,
            r#"{"api_key": "placeholder-not-a-real-key", "moods": [1, 2]}"#,
        ] {
            let config: Config = serde_json::from_str(raw)
                .unwrap_or_else(|error| panic!("moods 非法不应拖垮整份配置: raw={raw} error={error}"));
            assert!(config.moods.is_none(), "非法 moods 应按空处理: {raw}");
            assert_eq!(
                config.api_key.as_deref(),
                Some("placeholder-not-a-real-key"),
                "不得丢失同文件的 api_key: {raw}"
            );
        }
    }

    #[test]
    fn moods_field_roundtrips_and_is_omitted_when_none() {
        let with_moods = Config {
            moods: Some(MoodsConfig {
                custom: vec!["摸鱼".to_string()],
                hidden: vec!["走神".to_string()],
            }),
            ..Config::default()
        };
        let value = serde_json::to_value(&with_moods).expect("序列化");
        assert_eq!(
            value["moods"],
            serde_json::json!({ "custom": ["摸鱼"], "hidden": ["走神"] })
        );
        let parsed: Config = serde_json::from_value(value).expect("反序列化");
        assert_eq!(parsed, with_moods);

        // 没有 moods 时序列化不写出该字段，保持旧版 config.json 形状
        let without = Config::default();
        let value = serde_json::to_value(&without).expect("序列化");
        assert!(value.get("moods").is_none());
    }

    #[test]
    fn save_and_load_from_temp_file_preserves_all_fields() {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "desktop-narrator-config-save-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("创建测试临时目录");
        let path = dir.join("config.json");

        let config = Config {
            api_key: Some("placeholder-not-a-real-key".to_string()),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            moods: Some(MoodsConfig {
                custom: vec!["摸鱼".to_string()],
                hidden: vec!["想躲".to_string()],
            }),
            heading_presets: Some(vec![HeadingPreset {
                level: 2,
                text: "问题".to_string(),
            }]),
        };
        config.save_to(&path).expect("写入测试配置");
        let reloaded = Config::load_from(&path);
        assert_eq!(reloaded, config);

        let raw = std::fs::read_to_string(&path).expect("读测试配置");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("配置应是 JSON");
        assert_eq!(value["api_key"], "placeholder-not-a-real-key");
        assert_eq!(value["base_url"], DEFAULT_BASE_URL);
        assert_eq!(value["model"], DEFAULT_MODEL);
        assert_eq!(value["moods"]["custom"], serde_json::json!(["摸鱼"]));
        assert_eq!(
            value["heading_presets"],
            serde_json::json!([{ "level": 2, "text": "问题" }])
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- R15：heading_presets 可选字段与兼容性 ----

    #[test]
    fn legacy_config_without_heading_presets_field_loads() {
        // 旧版 config.json（124B 形态，无 heading_presets 字段）：不因缺字段报错
        let raw = r#"{
  "api_key": "placeholder-not-a-real-key",
  "base_url": "https://api.deepseek.com",
  "model": "deepseek-chat"
}"#;
        let config: Config = serde_json::from_str(raw).expect("旧版配置应能加载");
        assert_eq!(config.api_key.as_deref(), Some("placeholder-not-a-real-key"));
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.model, DEFAULT_MODEL);
        assert!(config.heading_presets.is_none());
    }

    #[test]
    fn invalid_heading_presets_field_is_treated_as_missing() {
        for raw in [
            r#"{"api_key": "placeholder-not-a-real-key", "heading_presets": "not-an-array"}"#,
            r#"{"api_key": "placeholder-not-a-real-key", "heading_presets": null}"#,
            r#"{"api_key": "placeholder-not-a-real-key", "heading_presets": 42}"#,
            r#"{"api_key": "placeholder-not-a-real-key", "heading_presets": [1, 2]}"#,
            r#"{"api_key": "placeholder-not-a-real-key", "moods": {"custom": ["摸鱼"]}, "heading_presets": [{"level": "bad", "text": "问题"}]}"#,
        ] {
            let config: Config = serde_json::from_str(raw).unwrap_or_else(|error| {
                panic!("heading_presets 非法不应拖垮整份配置: raw={raw} error={error}")
            });
            assert!(
                config.heading_presets.is_none(),
                "非法 heading_presets 应按缺失处理: {raw}"
            );
            assert_eq!(
                config.api_key.as_deref(),
                Some("placeholder-not-a-real-key"),
                "不得丢失同文件的 api_key: {raw}"
            );
        }
    }

    #[test]
    fn heading_presets_field_roundtrips_and_is_omitted_when_none() {
        let presets = vec![
            HeadingPreset {
                level: 2,
                text: "问题".to_string(),
            },
            HeadingPreset {
                level: 4,
                text: "复盘".to_string(),
            },
        ];
        let with_presets = Config {
            heading_presets: Some(presets.clone()),
            ..Config::default()
        };
        let value = serde_json::to_value(&with_presets).expect("序列化");
        assert_eq!(
            value["heading_presets"],
            serde_json::json!([
                { "level": 2, "text": "问题" },
                { "level": 4, "text": "复盘" }
            ])
        );
        let parsed: Config = serde_json::from_value(value).expect("反序列化");
        assert_eq!(parsed.heading_presets, Some(presets));

        // 没有 heading_presets 时序列化不写出该字段，保持旧版 config.json 形状
        let without = Config::default();
        let value = serde_json::to_value(&without).expect("序列化");
        assert!(value.get("heading_presets").is_none());
    }
}
