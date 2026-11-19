//! 桌面旁白记录器 —— 应用入口与外壳（P0）。
//!
//! 职责：单实例、托盘、主窗口生命周期（关闭/最小化 → 隐藏到托盘）、
//! 设置/提示的基础设施接线、共享模块初始化、IPC 命令注册、跨天事件。
//!
//! 边界：记录页实现在 P1（`records.rs` + `src/records/**`），
//! 生成引擎在 P2（`generator/**`）；本文件只调用它们的挂载点，不写业务实现。

mod commands;
mod config;
mod day;
mod generator;
mod heading_presets;
mod logging;
mod moods;
mod paths;
mod record_line;
mod records;
mod toast;
mod tray;

use tauri::{Emitter, Manager, RunEvent};

/// 主窗口 label（与 `tauri.conf.json` 一致）。
pub const MAIN_WINDOW_LABEL: &str = "main";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // 单实例插件必须最先注册：第二次启动只唤起已有窗口，不出现第二个托盘
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            logging::info("检测到第二次启动：唤起已有主窗口");
            tray::show_main_window(app);
        }))
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::save_api_key,
            commands::show_toast,
            commands::get_moods,
            commands::add_mood,
            commands::delete_mood,
            commands::get_heading_presets,
            commands::add_heading_preset,
            commands::update_heading_preset,
            commands::delete_heading_preset,
            records::get_today_document,
            records::save_today_document
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // 1) 路径与日志最先就绪，后面所有初始化都能写日志
            let app_paths = paths::init();
            logging::init(&app_paths.logs_dir);
            logging::info(format!(
                "应用启动 v{}（数据目录: {}）",
                app.package_info().version,
                app_paths.root.display()
            ));
            for warning in paths::init_warnings() {
                logging::warn(warning);
            }

            // 2) 配置（缺失时写出默认 config.json）
            let config = config::Config::load();
            let settings_view = config.settings_view();
            logging::info(format!(
                "配置加载完成（has_api_key={}，base_url={}，model={}）",
                settings_view.has_api_key, settings_view.base_url, settings_view.model
            ));

            // 3) 心情词 + 预设标题缓存（R14/R15：启动加载一次；增删改由 IPC 命令写回配置并更新内存）
            moods::init(&config);
            heading_presets::init(&config);

            // 4) 屏底提示窗口 + 托盘（失败只记日志，不阻断启动）
            if let Err(error) = toast::init(&handle) {
                logging::error(format!("屏底提示窗口初始化失败: {error}"));
            }
            if let Err(error) = tray::create(&handle) {
                logging::error(format!("托盘初始化失败: {error}"));
            }

            // 5) P2 生成引擎挂载点（当前为空实现）
            generator::init(&handle);

            // 6) 06:00 跨天事件（P1 监听后清空/重载记录列表）
            spawn_day_changed_loop(handle.clone());

            // 启动默认显示主窗口
            if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
                let _ = window.set_focus();
            } else {
                logging::warn("找不到主窗口（检查 tauri.conf.json 的 windows 配置）");
            }

            Ok(())
        })
        .on_window_event(|window, event| match event {
            // 关闭按钮：隐藏到托盘，进程保留
            tauri::WindowEvent::CloseRequested { api, .. } => {
                if window.label() == MAIN_WINDOW_LABEL {
                    api.prevent_close();
                    if let Err(error) = window.hide() {
                        logging::warn(format!("隐藏主窗口失败: {error}"));
                    }
                    logging::info("主窗口关闭请求：已隐藏到托盘（进程保留）");
                }
            }
            // 最小化：Windows 上最小化会触发 0×0 的 Resized 事件，据此隐藏到托盘
            tauri::WindowEvent::Resized(size) => {
                if window.label() != MAIN_WINDOW_LABEL {
                    return;
                }
                let minimized = window.is_minimized().unwrap_or(false);
                if minimized || (size.width == 0 && size.height == 0) {
                    if window.is_visible().unwrap_or(false) {
                        if let Err(error) = window.hide() {
                            logging::warn(format!("最小化隐藏主窗口失败: {error}"));
                        }
                        logging::info("主窗口最小化：已隐藏到托盘（进程保留）");
                    }
                }
            }
            _ => {}
        })
        .build(tauri::generate_context!())
        .expect("构建 Tauri 应用失败")
        .run(|_app, event| {
            if let RunEvent::Exit = event {
                logging::info("应用退出");
            }
        });
}

/// 每天本地时间 06:00 发一次 `day-changed`（payload 为新一天的 `day_key` 字符串）。
fn spawn_day_changed_loop(app: tauri::AppHandle) {
    std::thread::spawn(move || loop {
        let now = chrono::Local::now();
        let next = day::next_boundary_after(now);
        let wait_ms = (next - now).num_milliseconds().max(1_000) as u64;
        std::thread::sleep(std::time::Duration::from_millis(wait_ms));

        let key = day::day_key(chrono::Local::now());
        logging::info(format!("跨天：发出 day-changed（新的一天 {key}）"));
        if let Err(error) = app.emit("day-changed", key) {
            logging::warn(format!("发送 day-changed 事件失败: {error}"));
        }
    });
}
