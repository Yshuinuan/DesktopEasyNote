//! P0 负责的 IPC 命令：设置读取 / 保存 apikey / 屏底提示 / 自定义心情（R14）/ 预设标题（R15）。
//! P1/P2 的命令分别在 `records.rs`、未来的 `generator/` 里注册（注册表在 lib.rs）。

use tauri::AppHandle;

use crate::config::{Config, SettingsView};
use crate::heading_presets::{self, HeadingPreset};
use crate::moods::{self, MoodsView};
use crate::toast;

/// 读取设置视图。永远不返回 apikey 明文。
#[tauri::command]
pub fn get_settings() -> SettingsView {
    Config::load().settings_view()
}

/// 保存 apikey。空值/写盘失败返回可展示的错误；值不进日志。
#[tauri::command]
pub fn save_api_key(key: String) -> Result<(), String> {
    Config::save_api_key(&key)
}

/// 屏底提示（前端/调试用；P2 直接调用 Rust 函数 `toast::show_toast`）。
#[tauri::command]
pub fn show_toast(app: AppHandle, message: String) -> Result<(), String> {
    toast::show_toast(&app, &message)
}

/// 读取心情视图（可见/默认/自定义/隐藏），供右键心情菜单渲染。
#[tauri::command]
pub fn get_moods() -> Result<MoodsView, String> {
    Ok(moods::view())
}

/// 添加自定义心情；命中已隐藏的词视为恢复。失败返回可读字符串（前端原地显示）。
#[tauri::command]
pub fn add_mood(word: String) -> Result<MoodsView, String> {
    moods::add(&word)
}

/// 删除心情（仅隐藏，默认词同样适用；历史记录仍能识别）。失败返回可读字符串。
#[tauri::command]
pub fn delete_mood(word: String) -> Result<MoodsView, String> {
    moods::delete(&word)
}

/// 读取预设标题列表（数组顺序；默认四条在前、新增追加末尾）。
#[tauri::command]
pub fn get_heading_presets() -> Result<Vec<HeadingPreset>, String> {
    Ok(heading_presets::list())
}

/// 新增一条预设标题（追加到末尾）。成功返回操作后的完整列表。
#[tauri::command]
pub fn add_heading_preset(text: String, level: u8) -> Result<Vec<HeadingPreset>, String> {
    heading_presets::add(&text, level)
}

/// 按索引（0 起）修改一条预设标题，位置不变。成功返回操作后的完整列表。
#[tauri::command]
pub fn update_heading_preset(
    index: usize,
    text: String,
    level: u8,
) -> Result<Vec<HeadingPreset>, String> {
    heading_presets::update(index, &text, level)
}

/// 按索引（0 起）删除一条预设标题。成功返回操作后的完整列表。
#[tauri::command]
pub fn delete_heading_preset(index: usize) -> Result<Vec<HeadingPreset>, String> {
    heading_presets::delete(index)
}
