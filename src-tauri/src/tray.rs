//! 托盘图标、托盘菜单与主窗口唤起。
//!
//! - 左键点击托盘 → 显示并聚焦主窗口
//! - 右键菜单 → 「设置key」「退出」（debug 构建额外多一项「测试提示（调试）」）
//! - 只有「退出」真正结束进程；关闭/最小化主窗口由 `lib.rs` 拦截为隐藏

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};

use crate::logging;

pub const TRAY_SETTINGS_ID: &str = "settings";
pub const TRAY_QUIT_ID: &str = "quit";
pub const TRAY_DEBUG_TOAST_ID: &str = "debug-toast";

/// 显示并聚焦主窗口；若窗口是最小化状态则先还原。
pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(crate::MAIN_WINDOW_LABEL) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    } else {
        logging::warn("主窗口不存在，无法唤起");
    }
}

/// 创建托盘图标与菜单。失败返回错误字符串（启动流程只记日志、不中断）。
pub fn create(app: &AppHandle) -> Result<(), String> {
    let settings_item = MenuItem::with_id(app, TRAY_SETTINGS_ID, "设置key", true, None::<&str>)
        .map_err(|error| format!("创建「设置key」菜单项失败: {error}"))?;
    let quit_item = MenuItem::with_id(app, TRAY_QUIT_ID, "退出", true, None::<&str>)
        .map_err(|error| format!("创建「退出」菜单项失败: {error}"))?;

    #[cfg(debug_assertions)]
    let debug_toast_item = MenuItem::with_id(
        app,
        TRAY_DEBUG_TOAST_ID,
        "测试提示（调试）",
        true,
        None::<&str>,
    )
    .map_err(|error| format!("创建调试菜单项失败: {error}"))?;

    #[cfg(debug_assertions)]
    let menu = Menu::with_items(app, &[&settings_item, &debug_toast_item, &quit_item]);
    #[cfg(not(debug_assertions))]
    let menu = Menu::with_items(app, &[&settings_item, &quit_item]);
    let menu = menu.map_err(|error| format!("创建托盘菜单失败: {error}"))?;

    let mut builder = TrayIconBuilder::new()
        .tooltip("桌面旁白记录器")
        .menu(&menu)
        // 左键不弹菜单，交给 on_tray_icon_event 打开主窗口；右键仍会弹出菜单
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            TRAY_SETTINGS_ID => {
                logging::info("托盘菜单：设置key");
                show_main_window(app);
                if let Err(error) = app.emit("open-settings", ()) {
                    logging::warn(format!("发送 open-settings 事件失败: {error}"));
                }
            }
            TRAY_QUIT_ID => {
                logging::info("托盘菜单：退出");
                app.exit(0);
            }
            TRAY_DEBUG_TOAST_ID => {
                if let Err(error) = crate::toast::show_toast(app, "测试提示") {
                    logging::error(format!("调试触发屏底提示失败: {error}"));
                }
            }
            other => logging::debug(format!("未处理的托盘菜单项: {other}")),
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    } else {
        logging::warn("应用没有默认窗口图标，托盘图标将使用系统默认图标");
    }

    builder
        .build(app)
        .map_err(|error| format!("创建托盘图标失败: {error}"))?;

    logging::info("托盘初始化完成（左键开窗；右键菜单：设置key / 退出）");
    Ok(())
}
