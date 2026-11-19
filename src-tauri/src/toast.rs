//! 屏底提示基础设施。
//!
//! 设计：`toast` 是一个独立、无边框、置顶、透明、点击穿透、不进任务栏的常驻窗口，
//! 从启动起就显示在**主显示器工作区底部居中**（工作区已排除任务栏），但内容全透明，
//! 用户看不到任何白底。Rust 侧只负责定位与发事件，滑入/停留/缩回的动画由
//! `src/toast.ts` 用 CSS 完成——这样显示提示时不需要 `show()`，不会抢焦点、不会闪白。
//!
//! P2 直接调用：`crate::toast::show_toast(&app_handle, "今天的旁白写好了")`。

use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WebviewWindow};

use crate::logging;

pub const TOAST_WINDOW_LABEL: &str = "toast";

/// 提示窗口底边与工作区底边的间距（逻辑像素），避免贴住任务栏。
const BOTTOM_GAP_LOGICAL: f64 = 8.0;

#[derive(Clone, serde::Serialize)]
pub struct ToastPayload {
    pub message: String,
}

/// 启动时调用：设置点击穿透、不可聚焦，并定位到主屏底部。
pub fn init(app: &AppHandle) -> Result<(), String> {
    let window = toast_window(app)?;

    // 不可点击：鼠标事件穿透到下面的窗口
    window
        .set_ignore_cursor_events(true)
        .map_err(|error| format!("设置提示窗口点击穿透失败: {error}"))?;
    // 不可聚焦：常驻窗口不应抢走主窗口/其他应用的焦点
    window
        .set_focusable(false)
        .map_err(|error| format!("设置提示窗口不可聚焦失败: {error}"))?;
    position_bottom_center(&window)?;

    logging::info("屏底提示窗口就绪（透明/置顶/点击穿透/不抢焦点）");
    Ok(())
}

/// 显示一条屏底提示：重复调用会覆盖文字并重新计时（计时在前端 CSS 动画里）。
pub fn show_toast(app: &AppHandle, message: &str) -> Result<(), String> {
    let window = toast_window(app)?;

    // 显示器分辨率/DPI 可能变化，每次触发前重新定位一次
    if let Err(error) = position_bottom_center(&window) {
        crate::log_warn!("屏底提示定位失败（继续显示）: {error}");
    }

    window
        .emit(
            "toast-show",
            ToastPayload {
                message: message.to_string(),
            },
        )
        .map_err(|error| format!("发送提示事件失败: {error}"))?;

    let preview: String = message.chars().take(120).collect();
    logging::info(format!("屏底提示: {preview}"));
    Ok(())
}

fn toast_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    app.get_webview_window(TOAST_WINDOW_LABEL)
        .ok_or_else(|| format!("找不到 {TOAST_WINDOW_LABEL} 窗口（检查 tauri.conf.json）"))
}

fn position_bottom_center(window: &WebviewWindow) -> Result<(), String> {
    let Some(monitor) = window
        .primary_monitor()
        .map_err(|error| format!("读取主显示器失败: {error}"))?
    else {
        crate::log_warn!("未找到主显示器，屏底提示保持默认位置");
        return Ok(());
    };

    let work_area = monitor.work_area();
    let window_size = window
        .outer_size()
        .map_err(|error| format!("读取提示窗口尺寸失败: {error}"))?;

    let gap = (BOTTOM_GAP_LOGICAL * monitor.scale_factor()).round() as i32;
    let x = work_area.position.x + (work_area.size.width as i32 - window_size.width as i32) / 2;
    let y = work_area.position.y + work_area.size.height as i32 - window_size.height as i32 - gap;

    window
        .set_position(PhysicalPosition::new(x, y))
        .map_err(|error| format!("设置提示窗口位置失败: {error}"))?;
    Ok(())
}
